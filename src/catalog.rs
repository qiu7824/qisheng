//! Local library and a loopback-only bridge for legal, synthetic demo audio.
//!
//! This provider owns search. LX custom sources only resolve music URLs.

use crate::domain::library::{FileIdentity, LibraryPreparedAsset, LocalAssetAuthority};
use crate::domain::music::{LocalAsset, PlatformTrackRef, TrackRecord};
use crate::library::authority::{AuthorityRegistry, AuthorizedFile, FileAuthorization};
use crate::library::metadata::MAX_METADATA_READ_BYTES;
use anyhow::{Context, Result, bail, ensure};
use base64::Engine;
use md5::{Digest, Md5};
use rodio::{Decoder, Source};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::f64::consts::TAU;
use std::io::Cursor;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::thread;
use std::time::{Duration, Instant};

mod handover;
use handover::CatalogServerOwner;

pub const MAX_AUDIO_BYTES: usize = 64 * 1024 * 1024;
const SAMPLE_RATE: u32 = 22_050;
const MAX_RESOLVE_BODY_BYTES: usize = 2048;
const MAX_LOCAL_PROBE_SNAPSHOT_BYTES: usize = 64 * 1024;
const RESOLVE_FIXTURE_HEADER: &str = "lx-http-utils-v1";

fn open_authorized_request(authorization: &FileAuthorization) -> Result<AuthorizedFile> {
    // A same-root rename can precede the watcher's checked locator update.
    // Retry only a missing name through the same authority; every attempt
    // rechecks revocation and physical identity and never opens a raw path.
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match authorization.open() {
            Err(error)
                if Instant::now() < deadline
                    && error.chain().any(|cause| {
                        cause
                            .downcast_ref::<std::io::Error>()
                            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                    }) =>
            {
                thread::sleep(Duration::from_millis(10))
            }
            result => return result,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: String,
    pub title: String,
    pub artist: String,
    #[serde(default)]
    pub artists: Vec<String>,
    pub album: String,
    pub duration_seconds: f64,
    pub source_url: String,
    pub extension: String,
    pub is_demo: bool,
}

impl Track {
    /// The flattened legacy shape passed by upstream `toOldMusicInfo`.
    /// `filePath` is the explicit loopback bridge URL, not a private disk path.
    pub fn legacy_music_info(&self) -> Value {
        json!({
            "name": self.title,
            "singer": self.artist,
            "source": "local",
            "songmid": self.id,
            "interval": format_duration(self.duration_seconds),
            "albumName": self.album,
            "img": "",
            "typeUrl": {},
            "albumId": "",
            "types": [],
            "_types": {},
            "filePath": self.source_url,
            "ext": self.extension,
        })
    }

    pub fn request_info(&self) -> Value {
        json!({"type": null, "musicInfo": self.legacy_music_info()})
    }
}

pub fn format_duration(seconds: f64) -> String {
    let seconds = if seconds.is_finite() {
        seconds.max(0.0) as u64
    } else {
        0
    };
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[derive(Debug)]
struct SyntheticAudio {
    frequency: f64,
    duration_seconds: f64,
    bytes: OnceLock<Vec<u8>>,
}
impl SyntheticAudio {
    fn len(&self) -> u64 {
        let samples = (self.duration_seconds * SAMPLE_RATE as f64) as u32;
        44 + u64::from(samples) * 2
    }
    fn bytes(&self) -> &[u8] {
        self.bytes
            .get_or_init(|| synthesized_wav(self.frequency, self.duration_seconds))
    }
}

#[derive(Clone, Debug)]
enum AssetData {
    Synthetic(Arc<SyntheticAudio>),
    File {
        path: PathBuf,
        size: u64,
        modified: Option<std::time::SystemTime>,
        authorization: Option<FileAuthorization>,
    },
}

#[derive(Clone, Debug)]
struct Asset {
    data: AssetData,
    mime: &'static str,
}

/// Validated local metadata staged without changing the live catalog.
#[derive(Debug)]
pub struct PreparedLocalImport {
    path: PathBuf,
    track: Track,
    route: String,
    asset: Asset,
    warnings: Vec<String>,
    file_identity: Option<FileIdentity>,
    tags_verified: bool,
    metadata_preserved: bool,
    logical_identity: Option<LocalLogicalIdentity>,
}

#[derive(Debug)]
struct LocalLogicalIdentity {
    asset_id: String,
    lx_id: String,
}

impl PreparedLocalImport {
    pub fn track(&self) -> &Track {
        &self.track
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Whether the current file's tags were successfully parsed.
    pub fn tags_verified(&self) -> bool {
        self.tags_verified
    }

    /// Applies repository-allocated identities without changing the opened file
    /// or treating a logical identity as a filesystem authorization.
    pub fn bind_library_identity(&mut self, item: &LibraryPreparedAsset) -> Result<()> {
        let physical = self.library_asset();
        ensure!(
            item.asset.path == physical.asset.path
                && item.asset.size_bytes == physical.asset.size_bytes
                && item.asset.modified_ms == physical.asset.modified_ms
                && item.asset.extension == physical.asset.extension
                && item.asset.fingerprint == physical.asset.fingerprint
                && item.file_identity == self.file_identity,
            "Logical identity does not describe the prepared file"
        );
        ensure!(
            !item.track.id.is_empty()
                && item.track.id.len() <= 1024
                && !item
                    .track
                    .id
                    .chars()
                    .any(|character| matches!(character, '/' | '\\' | '?' | '#' | '\0'))
                && !item.track.lx_id.is_empty()
                && !item.asset.id.is_empty()
                && item.track.id == item.asset.track_id
                && item.track.local_asset_id.as_deref() == Some(item.asset.id.as_str())
                && item.track.platform.source == "local",
            "Local logical identities are inconsistent"
        );
        let base_url = self
            .track
            .source_url
            .strip_suffix(&self.route)
            .context("Prepared local route is inconsistent")?
            .to_owned();
        self.track.id.clone_from(&item.track.id);
        self.route = format!("/{}.{}", item.track.id, self.track.extension);
        self.track.source_url = format!("{base_url}{}", self.route);
        self.logical_identity = Some(LocalLogicalIdentity {
            asset_id: item.asset.id.clone(),
            lx_id: item.track.lx_id.clone(),
        });
        Ok(())
    }

    /// Keeps established display metadata when the new tag parser failed.
    /// The new file representation, route, authorization and identity remain
    /// the ones that were actually prepared and probed.
    pub fn preserve_metadata_from(&mut self, record: &TrackRecord) -> Result<()> {
        ensure!(
            record.id == self.track.id,
            "Preserved metadata belongs to another track"
        );
        self.track.title.clone_from(&record.title);
        self.track.artists.clone_from(&record.artists);
        self.track.artist = if record.artists.is_empty() {
            "本地音频".into()
        } else {
            record.artists.join(" / ")
        };
        self.track.album.clone_from(&record.album);
        if self.track.duration_seconds <= 0.0
            && let Some(duration) = record.duration_ms
        {
            self.track.duration_seconds = duration as f64 / 1000.0;
        }
        self.metadata_preserved = true;
        Ok(())
    }

    pub fn owned_metadata_bytes(&self) -> usize {
        let track = &self.track;
        let mut total =
            std::mem::size_of::<Self>() + self.path.capacity() * 2 + self.route.capacity();
        total += [
            &track.id,
            &track.title,
            &track.artist,
            &track.album,
            &track.source_url,
            &track.extension,
        ]
        .iter()
        .map(|value| value.capacity())
        .sum::<usize>();
        total += track.artists.capacity() * std::mem::size_of::<String>()
            + track.artists.iter().map(String::capacity).sum::<usize>();
        total += self.warnings.capacity() * std::mem::size_of::<String>()
            + self.warnings.iter().map(String::capacity).sum::<usize>();
        if let Some(identity) = &self.logical_identity {
            total += std::mem::size_of::<LocalLogicalIdentity>()
                + identity.asset_id.capacity()
                + identity.lx_id.capacity();
        }
        if let AssetData::File { path, .. } = &self.asset.data {
            total += path.capacity() * 2;
        }
        total
    }

    pub fn set_authorization(&mut self, authorization: FileAuthorization) {
        if let AssetData::File {
            authorization: current,
            ..
        } = &mut self.asset.data
        {
            *current = Some(authorization);
        }
    }

    pub fn library_asset(&self) -> LibraryPreparedAsset {
        let (size, modified) = match &self.asset.data {
            AssetData::File { size, modified, .. } => (*size, *modified),
            AssetData::Synthetic(_) => unreachable!("local imports have file data"),
        };
        let mut asset = LocalAsset::new(self.track.id.clone(), self.path.clone(), size);
        asset.fingerprint = self.file_identity.as_ref().map(|identity| {
            format!(
                "{:x}",
                sha2::Sha256::digest(
                    serde_json::to_vec(identity).expect("file identity is serializable")
                )
            )
        });
        asset.modified_ms = modified
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        let path_text = asset.path.to_string_lossy().into_owned();
        let mut track = TrackRecord {
            id: self.track.id.clone(),
            lx_id: path_text.clone(),
            title: self.track.title.clone(),
            artists: if self.track.artists.is_empty() && !self.metadata_preserved {
                vec![self.track.artist.clone()]
            } else {
                self.track.artists.clone()
            },
            album: self.track.album.clone(),
            duration_ms: Some((self.track.duration_seconds * 1000.0) as u64),
            platform: PlatformTrackRef {
                source: "local".into(),
                song_id: Value::String(path_text.clone()),
                qualities: Vec::new(),
            },
            local_asset_id: Some(asset.id.clone()),
            metadata: json!({"songId":path_text,"filePath":path_text,"albumName":self.track.album})
                .as_object()
                .unwrap()
                .clone(),
            wire: None,
        };
        if let Some(identity) = &self.logical_identity {
            asset.id.clone_from(&identity.asset_id);
            track.local_asset_id = Some(asset.id.clone());
            track.lx_id.clone_from(&identity.lx_id);
        }
        LibraryPreparedAsset {
            track,
            asset,
            file_identity: self.file_identity.clone(),
        }
    }
}

struct LocalPreparation {
    path: PathBuf,
    metadata: std::fs::Metadata,
    extension: String,
    mime: &'static str,
    authorization: Option<FileAuthorization>,
    file_identity: Option<FileIdentity>,
}

/// The decoder may discard an underlying I/O error while searching formats or
/// producing its first sample. A shared sticky flag preserves budget failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalProbeBudgetExceeded {
    pub limit_bytes: usize,
}
impl std::fmt::Display for LocalProbeBudgetExceeded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "Local audio probe exceeded its {}-byte read budget",
            self.limit_bytes
        )
    }
}
impl std::error::Error for LocalProbeBudgetExceeded {}

struct ProbeReader<R> {
    reader: R,
    remaining: usize,
    limit: usize,
    exceeded: Arc<AtomicBool>,
}
impl<R> ProbeReader<R> {
    fn new(reader: R, limit: usize) -> Self {
        Self {
            reader,
            remaining: limit,
            limit,
            exceeded: Arc::new(AtomicBool::new(false)),
        }
    }
}
impl<R: Read> Read for ProbeReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            self.exceeded.store(true, Ordering::Release);
            return Err(std::io::Error::other(LocalProbeBudgetExceeded {
                limit_bytes: self.limit,
            }));
        }
        let limit = buffer.len().min(self.remaining).min(64 * 1024);
        let count = self.reader.read(&mut buffer[..limit])?;
        self.remaining -= count;
        Ok(count)
    }
}
impl<R: Seek> Seek for ProbeReader<R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.reader.seek(position)
    }
}

fn probe_local_audio<R: Read + Seek + Send + Sync + 'static>(
    reader: R,
    length: u64,
) -> Result<f64> {
    probe_local_audio_with_limit(reader, length, MAX_METADATA_READ_BYTES)
}
fn probe_local_audio_with_limit<R: Read + Seek + Send + Sync + 'static>(
    reader: R,
    length: u64,
    limit: usize,
) -> Result<f64> {
    let reader = ProbeReader::new(reader, limit);
    let exceeded = reader.exceeded.clone();
    let decoded = Decoder::builder()
        .with_data(reader)
        .with_byte_len(length)
        .with_seekable(true)
        .build();
    ensure!(
        !exceeded.load(Ordering::Acquire),
        LocalProbeBudgetExceeded { limit_bytes: limit }
    );
    let mut decoder = decoded.context("音频文件损坏或编码不受支持")?;
    let duration = decoder
        .total_duration()
        .map_or(0.0, |duration| duration.as_secs_f64());
    let sample = decoder.next();
    ensure!(
        !exceeded.load(Ordering::Acquire),
        LocalProbeBudgetExceeded { limit_bytes: limit }
    );
    ensure!(
        sample.is_some_and(|sample| sample.is_finite()),
        "音频文件没有有效的可解码样本"
    );
    Ok(duration)
}

#[cfg(test)]
mod local_probe_tests {
    use super::*;
    use std::io;

    #[test]
    fn rereading_after_seek_consumes_actual_read_budget_and_exhaustion_is_typed_error() {
        let mut reader = ProbeReader::new(Cursor::new(b"abcdef".to_vec()), 10);
        let mut first = [0; 4];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"abcd");
        reader.seek(SeekFrom::Start(0)).unwrap();
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, b"abcd");
        reader.seek(SeekFrom::End(-2)).unwrap();
        let mut last = [0; 2];
        reader.read_exact(&mut last).unwrap();
        assert_eq!(&last, b"ef");
        reader.seek(SeekFrom::Start(0)).unwrap();
        let error = reader.read(&mut first).unwrap_err();
        assert!(error.get_ref().unwrap().is::<LocalProbeBudgetExceeded>());
        assert!(reader.exceeded.load(Ordering::Acquire));
        assert_eq!(reader.read(&mut []).unwrap(), 0);
    }

    #[test]
    fn large_seek_does_not_charge_budget_while_individual_reads_and_real_eof_remain_bounded() {
        let mut bytes = vec![0x55; 200_000];
        bytes[199_997..].copy_from_slice(b"end");
        let mut reader = ProbeReader::new(Cursor::new(bytes), 200_000);
        let mut buffer = vec![0; 130_000];
        assert_eq!(reader.read(&mut buffer).unwrap(), 64 * 1024);
        assert!(buffer[..64 * 1024].iter().all(|value| *value == 0x55));
        reader.seek(SeekFrom::End(-3)).unwrap();
        assert_eq!(reader.read(&mut buffer).unwrap(), 3);
        assert_eq!(&buffer[..3], b"end");
        assert_eq!(reader.read(&mut buffer).unwrap(), 0);
        reader.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(reader.read(&mut buffer).unwrap(), 64 * 1024);
        assert!(!reader.exceeded.load(Ordering::Acquire));
    }

    #[test]
    fn underlying_read_and_seek_errors_are_not_changed_to_eof_or_budget_failure() {
        struct FailingReader;
        impl Read for FailingReader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "fixture access denied",
                ))
            }
        }
        impl Seek for FailingReader {
            fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
                Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fixture seek rejected",
                ))
            }
        }
        let mut reader = ProbeReader::new(FailingReader, 8);
        assert_eq!(
            reader.read(&mut [0; 4]).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            reader.seek(SeekFrom::Start(0)).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert!(!reader.exceeded.load(Ordering::Acquire));
    }

    #[test]
    fn actual_wav_probe_decodes_a_sample_and_reports_swallowed_decoder_budget_failure() {
        let bytes = synthesized_wav(440.0, 0.1);
        let length = bytes.len() as u64;
        let duration = probe_local_audio(Cursor::new(bytes.clone()), length).unwrap();
        assert!((duration - 0.1).abs() < 0.001);
        let error = probe_local_audio_with_limit(Cursor::new(bytes), length, 16).unwrap_err();
        assert_eq!(
            error.downcast_ref::<LocalProbeBudgetExceeded>(),
            Some(&LocalProbeBudgetExceeded { limit_bytes: 16 })
        );
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn failed_real_wav_metadata_preserves_known_tags_without_replacing_the_new_file_representation()
    {
        let folder = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = folder.path().join("damaged-tags.wav");
        let mut bytes = synthesized_wav(330.0, 0.1);
        bytes.extend_from_slice(b"LIST");
        bytes.extend_from_slice(&1000u32.to_le_bytes());
        bytes.extend_from_slice(b"INFO");
        let riff_size = bytes.len() as u32 - 8;
        bytes[4..8].copy_from_slice(&riff_size.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        assert!(crate::library::metadata::read_local_metadata(&path).is_err());
        let catalog = Catalog::new().unwrap();
        let mut prepared = catalog.prepare_local(&path).unwrap();
        assert!(!prepared.tags_verified());
        assert!(!prepared.warnings().is_empty());
        assert!((prepared.track().duration_seconds - 0.1).abs() < 0.001);
        let original_file = prepared.library_asset();
        let original_route = prepared.track().source_url.clone();
        let original_path = prepared.path().to_path_buf();
        let mut old = original_file.track.clone();
        old.title = "Retained title".into();
        old.artists = vec!["Retained artist".into(), "Second artist".into()];
        old.album = "Retained album".into();
        old.duration_ms = Some(5000);
        prepared.preserve_metadata_from(&old).unwrap();
        let preserved = prepared.library_asset();
        assert_eq!(preserved.asset, original_file.asset);
        assert_eq!(preserved.file_identity, original_file.file_identity);
        assert_eq!(prepared.path(), original_path);
        assert_eq!(prepared.track().source_url, original_route);
        assert_eq!(preserved.track.id, original_file.track.id);
        assert_eq!(preserved.track.title, old.title);
        assert_eq!(preserved.track.artists, old.artists);
        assert_eq!(preserved.track.album, old.album);
        assert_eq!(preserved.track.duration_ms, Some(100));
        assert!(!prepared.tags_verified());

        prepared.track.duration_seconds = 0.0;
        old.artists.clear();
        prepared.preserve_metadata_from(&old).unwrap();
        assert!(prepared.library_asset().track.artists.is_empty());
        assert_eq!(prepared.library_asset().track.duration_ms, Some(5000));
        let title = prepared.track().title.clone();
        old.id = "wrong-track".into();
        old.title = "Must not be used".into();
        assert!(prepared.preserve_metadata_from(&old).is_err());
        assert_eq!(prepared.track().title, title);

        let clean_path = folder.path().join("clean.wav");
        std::fs::write(&clean_path, synthesized_wav(440.0, 0.1)).unwrap();
        assert!(catalog.prepare_local(clean_path).unwrap().tags_verified());
    }
}

enum CatalogFile {
    Authorized(AuthorizedFile),
    Selected(std::fs::File),
}
impl CatalogFile {
    fn metadata(&self) -> std::io::Result<std::fs::Metadata> {
        match self {
            Self::Authorized(file) => file.metadata().map_err(std::io::Error::other),
            Self::Selected(file) => file.metadata(),
        }
    }
    fn check(&self) -> std::io::Result<()> {
        match self {
            Self::Authorized(file) => file.check().map_err(std::io::Error::other),
            Self::Selected(_) => Ok(()),
        }
    }
}
impl Read for CatalogFile {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Self::Authorized(file) => file.read(bytes),
            Self::Selected(file) => file.read(bytes),
        }
    }
}
impl Seek for CatalogFile {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        match self {
            Self::Authorized(file) => file.seek(position),
            Self::Selected(file) => file.seek(position),
        }
    }
}

struct CatalogInner {
    base_url: String,
    tracks: Arc<RwLock<Vec<Track>>>,
    assets: Arc<RwLock<HashMap<String, Asset>>>,
    shutdown: Arc<AtomicBool>,
    revision: AtomicU64,
    server: Mutex<CatalogServerOwner>,
}

impl Drop for CatalogInner {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        self.server
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .join_on_drop();
    }
}

/// Clones share the local server, library and explicit local file references.
#[derive(Clone)]
pub struct Catalog {
    inner: Arc<CatalogInner>,
}

impl Catalog {
    pub fn new() -> Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).context("无法启动本地音频服务")?;
        listener.set_nonblocking(true)?;
        let base_url = format!("http://{}", listener.local_addr()?);
        let tracks = Arc::new(RwLock::new(Vec::new()));
        let assets = Arc::new(RwLock::new(HashMap::new()));
        let shutdown = Arc::new(AtomicBool::new(false));

        let demos = [
            ("tone-1", "First Light", "晨光 · 合成钢琴", 45.0, 261.63),
            ("tone-2", "Violet Hour", "暮色 · 合成和弦", 52.0, 220.0),
            ("tone-3", "Quiet Tide", "潮汐 · 合成氛围", 60.0, 174.61),
        ];
        for (id, title, album, duration_seconds, frequency) in demos {
            let route = format!("/{id}.wav");
            assets.write().unwrap().insert(
                route.clone(),
                Asset {
                    data: AssetData::Synthetic(Arc::new(SyntheticAudio {
                        frequency,
                        duration_seconds,
                        bytes: OnceLock::new(),
                    })),
                    mime: "audio/wav",
                },
            );
            tracks.write().unwrap().push(Track {
                id: id.into(),
                title: title.into(),
                artist: "LX Native Lab".into(),
                artists: vec!["LX Native Lab".into()],
                album: album.into(),
                duration_seconds,
                source_url: format!("{base_url}{route}"),
                extension: "wav".into(),
                is_demo: true,
            });
        }

        let server_tracks = tracks.clone();
        let server_assets = assets.clone();
        let server_shutdown = shutdown.clone();
        let server = thread::Builder::new()
            .name("lx-local-catalog".into())
            .spawn(move || {
                while !server_shutdown.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, address)) if address.ip().is_loopback() => {
                            let _ = serve_request(
                                &mut stream,
                                &server_tracks,
                                &server_assets,
                                &server_shutdown,
                            );
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error)
                            if matches!(
                                error.kind(),
                                std::io::ErrorKind::ConnectionAborted
                                    | std::io::ErrorKind::ConnectionReset
                                    | std::io::ErrorKind::Interrupted
                            ) => {}
                        Err(_) => break,
                    }
                }
            })
            .context("无法启动本地曲库线程")?;

        Ok(Self {
            inner: Arc::new(CatalogInner {
                base_url,
                tracks,
                assets,
                shutdown,
                revision: AtomicU64::new(0),
                server: Mutex::new(CatalogServerOwner::new(server)),
            }),
        })
    }

    pub fn base_url(&self) -> &str {
        &self.inner.base_url
    }

    pub fn manifest_url(&self) -> String {
        format!("{}/catalog.json", self.base_url())
    }

    pub fn revision(&self) -> u64 {
        self.inner.revision.load(Ordering::Acquire)
    }

    pub fn tracks(&self) -> Vec<Track> {
        self.inner.tracks.read().unwrap().clone()
    }

    pub fn track(&self, id: &str) -> Option<Track> {
        self.inner
            .tracks
            .read()
            .unwrap()
            .iter()
            .find(|track| track.id == id)
            .cloned()
    }

    pub fn search(&self, query: &str) -> Vec<Track> {
        let query = query.trim().to_lowercase();
        self.inner
            .tracks
            .read()
            .unwrap()
            .iter()
            .filter(|track| {
                query.is_empty()
                    || [&track.title, &track.artist, &track.album]
                        .iter()
                        .any(|field| field.to_lowercase().contains(&query))
            })
            .cloned()
            .collect()
    }
    pub(crate) fn has_download_read_authority(&self, id: &str) -> bool {
        let Some(track) = self.track(id) else {
            return false;
        };
        let Some(route) = track.source_url.strip_prefix(self.base_url()) else {
            return false;
        };
        self.inner.assets.read().unwrap_or_else(|error| error.into_inner()).get(route)
            .is_some_and(|asset| matches!(&asset.data, AssetData::File { authorization: Some(authorization), .. } if authorization.is_download_read()))
    }
    pub(crate) fn has_download_read_binding(&self, id: &str) -> bool {
        let Some(track) = self.track(id) else {
            return false;
        };
        let Some(route) = track.source_url.strip_prefix(self.base_url()) else {
            return false;
        };
        self.inner.assets.read().unwrap_or_else(|error| error.into_inner()).get(route)
            .is_some_and(|asset| matches!(&asset.data, AssetData::File { authorization: Some(authorization), .. } if authorization.has_download_read_binding()))
    }
    pub(crate) fn download_read_authorization(&self, id: &str) -> Option<FileAuthorization> {
        let track = self.track(id)?;
        let route = track.source_url.strip_prefix(self.base_url())?;
        let assets = self
            .inner
            .assets
            .read()
            .unwrap_or_else(|error| error.into_inner());
        match &assets.get(route)?.data {
            AssetData::File {
                authorization: Some(authorization),
                ..
            } if authorization.has_download_read_binding() => Some(authorization.clone()),
            _ => None,
        }
    }

    /// Compatibility entry point; callers with persistence use prepare then commit.
    pub fn import_local(&self, path: PathBuf) -> Result<Track> {
        let prepared = self.prepare_local(path)?;
        Ok(self.commit_local(prepared))
    }

    /// Reads and probes only an explicitly selected file, without mutating state.
    /// Run this phase off the control actor and persist its path before committing.
    pub fn prepare_local(&self, path: impl AsRef<Path>) -> Result<PreparedLocalImport> {
        let path = std::fs::canonicalize(path.as_ref()).context("无法定位所选音频文件")?;
        #[cfg(any(windows, target_os = "linux"))]
        {
            let selected_path = path.clone();
            let track_id = crate::domain::music::local_track_id(&path);
            let asset_id = LocalAsset::new(track_id.clone(), path.clone(), 0).id;
            let intent = LocalAssetAuthority {
                asset_id,
                track_id,
                path,
                explicit_file_epoch: Some(1),
                roots: Vec::new(),
            };
            let authorization = AuthorityRegistry::default().install(&intent, None)?;
            let mut prepared = self.prepare_authorized(authorization.open()?)?;
            prepared.path = selected_path;
            Ok(prepared)
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let (extension, mime) = local_audio_mime(&path)?;
            let file = std::fs::File::open(&path).context("无法读取所选音频文件")?;
            let metadata = file.metadata()?;
            ensure!(
                metadata.is_file() && metadata.len() > 0,
                "所选文件必须是非空普通音频文件"
            );
            let reader = file.try_clone()?;
            let duration = probe_local_audio(reader, metadata.len())?;
            let tags = crate::library::metadata::read_local_metadata_from_open_file(
                file,
                path.clone(),
                metadata.len(),
            );
            self.complete_local_preparation(
                LocalPreparation {
                    path,
                    metadata,
                    extension,
                    mime,
                    authorization: None,
                    file_identity: None,
                },
                duration,
                tags,
            )
        }
    }

    /// Prepares tags and samples through the same verified, revocable file handle.
    pub fn prepare_authorized(&self, file: AuthorizedFile) -> Result<PreparedLocalImport> {
        let initial = file.metadata_snapshot()?;
        let path = initial.path;
        let file_identity = Some(file.identity().clone());
        let authorization = Some(file.authorization());
        let metadata = initial.metadata;
        ensure!(
            metadata.is_file() && metadata.len() > 0,
            "所选文件必须是非空普通音频文件"
        );
        let (extension, mime) = local_audio_mime(&path)?;
        let (duration, tags) = if metadata.len() <= MAX_LOCAL_PROBE_SNAPSHOT_BYTES as u64 {
            let mut reader = file.try_clone()?;
            reader.seek(SeekFrom::Start(0))?;
            let mut bytes = vec![0; metadata.len() as usize];
            reader.read_exact(&mut bytes)?;
            let bytes: Arc<[u8]> = bytes.into();
            let duration = probe_local_audio(Cursor::new(bytes.clone()), metadata.len())?;
            let tags = crate::library::metadata::read_local_metadata_from_open_file(
                Cursor::new(bytes),
                path.clone(),
                metadata.len(),
            );
            (duration, tags)
        } else {
            let duration = probe_local_audio(file.try_clone()?, metadata.len())?;
            let tags = crate::library::metadata::read_local_metadata_from_open_file(
                file.try_clone()?,
                path.clone(),
                metadata.len(),
            );
            (duration, tags)
        };
        let final_state = file.metadata_snapshot()?;
        let final_metadata = final_state.metadata;
        ensure!(
            final_metadata.len() == metadata.len()
                && final_metadata.modified().ok() == metadata.modified().ok()
                && final_state.path == path
                && final_state.change_stamp == initial.change_stamp,
            "音频文件在准备期间发生变化"
        );
        self.complete_local_preparation(
            LocalPreparation {
                path,
                metadata,
                extension,
                mime,
                authorization,
                file_identity,
            },
            duration,
            tags,
        )
    }

    /// Restages verified probe metadata with a freshly proved file handle.
    /// Callers must compare their high-precision change-stamp cache key on
    /// both sides of this operation; metadata never supplies a read grant.
    pub fn prepare_authorized_from_verified(
        &self,
        file: AuthorizedFile,
        cached: &LibraryPreparedAsset,
    ) -> Result<PreparedLocalImport> {
        let actual = file.metadata_snapshot()?;
        let path = actual.path;
        let metadata = actual.metadata;
        let modified = metadata.modified().ok();
        let modified_ms = modified
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| u64::try_from(duration.as_millis()).ok());
        ensure!(
            metadata.is_file()
                && metadata.len() > 0
                && cached.file_identity.as_ref() == Some(file.identity())
                && crate::domain::music::normalize_local_path(&cached.asset.path)
                    == crate::domain::music::normalize_local_path(&path)
                && cached.asset.size_bytes == metadata.len()
                && cached.asset.modified_ms == modified_ms
                && cached.track.id == cached.asset.track_id
                && cached.track.local_asset_id.as_deref() == Some(cached.asset.id.as_str())
                && cached.track.platform.source == "local",
            "Verified local metadata cache key changed"
        );
        let (extension, mime) = local_audio_mime(&path)?;
        let route = format!("/{}.{}", cached.track.id, extension);
        let track = Track {
            id: cached.track.id.clone(),
            title: cached.track.title.clone(),
            artist: if cached.track.artists.is_empty() {
                "本地音频".into()
            } else {
                cached.track.artists.join(" / ")
            },
            artists: cached.track.artists.clone(),
            album: cached.track.album.clone(),
            duration_seconds: cached
                .track
                .duration_ms
                .map_or(0.0, |duration| duration as f64 / 1000.0),
            source_url: format!("{}{route}", self.base_url()),
            extension,
            is_demo: false,
        };
        file.check()?;
        Ok(PreparedLocalImport {
            path: path.clone(),
            track,
            route,
            asset: Asset {
                data: AssetData::File {
                    path,
                    size: metadata.len(),
                    modified,
                    authorization: Some(file.authorization()),
                },
                mime,
            },
            warnings: Vec::new(),
            file_identity: Some(file.identity().clone()),
            tags_verified: true,
            metadata_preserved: true,
            logical_identity: Some(LocalLogicalIdentity {
                asset_id: cached.asset.id.clone(),
                lx_id: cached.track.lx_id.clone(),
            }),
        })
    }

    fn complete_local_preparation(
        &self,
        preparation: LocalPreparation,
        mut duration_seconds: f64,
        tags: Result<crate::library::metadata::LocalMetadata>,
    ) -> Result<PreparedLocalImport> {
        let LocalPreparation {
            path,
            metadata,
            extension,
            mime,
            authorization,
            file_identity,
        } = preparation;
        let tags_verified = tags.is_ok();
        let (local_metadata, warnings) = match tags {
            Ok(metadata) => {
                let warnings = metadata.warnings.clone();
                (Some(metadata), warnings)
            }
            Err(error) => (None, vec![format!("音频标签无法读取：{error}")]),
        };
        if duration_seconds == 0.0 {
            duration_seconds = local_metadata
                .as_ref()
                .map_or(0.0, |metadata| metadata.duration_ms as f64 / 1000.0);
        }
        let id = crate::domain::music::local_track_id(&path);
        let route = format!("/{id}.{extension}");
        let track = Track {
            id,
            title: local_metadata
                .as_ref()
                .and_then(|metadata| metadata.title.clone())
                .unwrap_or_else(|| {
                    path.file_stem()
                        .and_then(|name| name.to_str())
                        .unwrap_or("本地音频")
                        .to_string()
                }),
            artist: local_metadata
                .as_ref()
                .filter(|metadata| !metadata.artists.is_empty())
                .map_or_else(
                    || "本地音频".into(),
                    |metadata| metadata.artists.join(" / "),
                ),
            artists: local_metadata
                .as_ref()
                .map_or_else(Vec::new, |metadata| metadata.artists.clone()),
            album: local_metadata
                .as_ref()
                .and_then(|metadata| metadata.album.clone())
                .unwrap_or_else(|| "用户导入".into()),
            duration_seconds,
            source_url: format!("{}{route}", self.base_url()),
            extension,
            is_demo: false,
        };
        let asset = Asset {
            data: AssetData::File {
                path: path.clone(),
                size: metadata.len(),
                modified: metadata.modified().ok(),
                authorization,
            },
            mime,
        };
        Ok(PreparedLocalImport {
            path,
            track,
            route,
            asset,
            warnings,
            file_identity,
            tags_verified,
            metadata_preserved: false,
            logical_identity: None,
        })
    }

    /// Publishes a validated import after durable storage succeeded, without I/O.
    /// Duplicate unchanged paths keep their entry, order and catalog revision.
    pub fn commit_local(&self, mut prepared: PreparedLocalImport) -> Track {
        prepared.track.source_url = format!("{}{}", self.base_url(), prepared.route);
        let mut tracks = self
            .inner
            .tracks
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut assets = self
            .inner
            .assets
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let index = tracks
            .iter()
            .position(|track| track.id == prepared.track.id);
        let same_asset = assets.get(&prepared.route).is_some_and(|previous| {
            match (&previous.data, &prepared.asset.data) {
                (
                    AssetData::File {
                        path: old_path,
                        size: old_size,
                        modified: old_modified,
                        ..
                    },
                    AssetData::File {
                        path,
                        size,
                        modified,
                        ..
                    },
                ) => old_path == path && old_size == size && old_modified == modified,
                _ => false,
            }
        });
        if index.is_some_and(|index| tracks[index] == prepared.track) && same_asset {
            assets.insert(prepared.route, prepared.asset);
            return tracks[index.unwrap()].clone();
        }
        let track = prepared.track;
        assets.insert(prepared.route, prepared.asset);
        if let Some(index) = index {
            tracks[index] = track.clone();
        } else {
            tracks.push(track.clone());
        }
        self.inner.revision.fetch_add(1, Ordering::Release);
        track
    }

    /// Retains the existing loopback locator when a relocation preserves the
    /// representation consumed by active readers and prepared playback.
    pub fn commit_rebound(
        &self,
        mut prepared: PreparedLocalImport,
        media_changed: bool,
    ) -> Result<Track> {
        ensure!(
            prepared.logical_identity.is_some(),
            "Relocated import has no stable logical identity"
        );
        if !media_changed && let Some(previous) = self.track(&prepared.track.id) {
            let base_url = self.base_url();
            prepared.route = previous
                .source_url
                .strip_prefix(base_url)
                .filter(|route| {
                    route.starts_with('/') && !route.contains('?') && !route.contains('#')
                })
                .context("Relocated catalog route is inconsistent")?
                .to_owned();
        }
        Ok(self.commit_local(prepared))
    }

    pub fn open_selected_file(path: impl AsRef<Path>) -> Result<AuthorizedFile> {
        let path = std::fs::canonicalize(path.as_ref()).context("无法定位所选音频文件")?;
        let track_id = crate::domain::music::local_track_id(&path);
        let asset_id = LocalAsset::new(track_id.clone(), path.clone(), 0).id;
        let intent = LocalAssetAuthority {
            asset_id,
            track_id,
            path,
            explicit_file_epoch: Some(1),
            roots: Vec::new(),
        };
        AuthorityRegistry::default().install(&intent, None)?.open()
    }

    /// Registers durable metadata and a revocable lazy file route without reading tags.
    pub fn restore_local_record(
        &self,
        record: &TrackRecord,
        asset: &LocalAsset,
        authorization: FileAuthorization,
    ) -> Result<Track> {
        ensure!(
            record.id == asset.track_id
                && record.local_asset_id.as_deref() == Some(asset.id.as_str()),
            "Local asset and track identities differ"
        );
        let (extension, mime) = local_audio_mime(&asset.path)?;
        let route = format!("/{}.{extension}", record.id);
        let track = Track {
            id: record.id.clone(),
            title: record.title.clone(),
            artist: if record.artists.is_empty() {
                "本地音频".into()
            } else {
                record.artists.join(" / ")
            },
            artists: record.artists.clone(),
            album: record.album.clone(),
            duration_seconds: record
                .duration_ms
                .map_or(0.0, |duration| duration as f64 / 1000.0),
            source_url: format!("{}{route}", self.base_url()),
            extension,
            is_demo: false,
        };
        let prepared = PreparedLocalImport {
            path: asset.path.clone(),
            track,
            route,
            asset: Asset {
                mime,
                data: AssetData::File {
                    path: asset.path.clone(),
                    size: asset.size_bytes,
                    modified: asset.modified_ms.and_then(|time| {
                        std::time::UNIX_EPOCH.checked_add(Duration::from_millis(time))
                    }),
                    authorization: Some(authorization),
                },
            },
            warnings: Vec::new(),
            file_identity: None,
            tags_verified: true,
            metadata_preserved: true,
            logical_identity: None,
        };
        Ok(self.commit_local(prepared))
    }

    pub fn remove_local_records(&self, track_ids: &[String]) {
        let ids = track_ids.iter().collect::<std::collections::HashSet<_>>();
        let mut tracks = self
            .inner
            .tracks
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let mut assets = self
            .inner
            .assets
            .write()
            .unwrap_or_else(|error| error.into_inner());
        let routes = tracks
            .iter()
            .filter(|track| !track.is_demo && ids.contains(&track.id))
            .filter_map(|track| {
                track
                    .source_url
                    .strip_prefix(self.base_url())
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        if routes.is_empty() {
            return;
        }
        for route in routes {
            assets.remove(&route);
        }
        tracks.retain(|track| track.is_demo || !ids.contains(&track.id));
        self.inner.revision.fetch_add(1, Ordering::Release);
    }
}

fn system_time_ms(time: Option<std::time::SystemTime>) -> Option<u64> {
    time.and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
}

fn local_audio_mime(path: &Path) -> Result<(String, &'static str)> {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let mime = match extension.as_str() {
        "wav" => "audio/wav",
        "mp3" => "audio/mpeg",
        "ogg" | "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "m4a" | "mp4" | "aac" => "audio/mp4",
        _ => bail!("不支持的音频格式；请选择 WAV、MP3、OGG、FLAC 或 M4A"),
    };
    Ok((extension, mime))
}

fn serve_request(
    stream: &mut TcpStream,
    tracks: &RwLock<Vec<Track>>,
    assets: &RwLock<HashMap<String, Asset>>,
    shutdown: &AtomicBool,
) -> std::io::Result<()> {
    // Winsock accept inherits the listener's nonblocking mode. Timeouts do not
    // undo that mode: a delayed header or full send buffer would yield WouldBlock
    // and close this otherwise valid connection before its response is complete.
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let header_deadline = Instant::now() + Duration::from_secs(2);
    let mut request = Vec::new();
    let mut chunk = [0_u8; 1024];
    let mut headers_complete = false;
    while request.len() < 8192 {
        if shutdown.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Catalog stopped",
            ));
        }
        let remaining = header_deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "HTTP request headers timed out",
                )
            })?;
        stream.set_read_timeout(Some(remaining))?;
        let read_limit = chunk.len().min(8192 - request.len());
        let length = stream.read(&mut chunk[..read_limit])?;
        if length == 0 {
            break;
        }
        request.extend_from_slice(&chunk[..length]);
        if request.windows(4).any(|window| window == b"\r\n\r\n") {
            headers_complete = true;
            break;
        }
    }
    if !headers_complete {
        return write_response(
            stream,
            if request.len() == 8192 {
                "431 Request Header Fields Too Large"
            } else {
                "400 Bad Request"
            },
            "text/plain",
            b"Complete HTTP request headers required",
            false,
        );
    }
    let header_end = request
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .unwrap();
    let headers = match std::str::from_utf8(&request[..header_end]) {
        Ok(headers) => headers,
        Err(_) => return write_json_error(stream, "400 Bad Request", "Invalid HTTP headers"),
    };
    let mut parts = headers
        .lines()
        .next()
        .unwrap_or_default()
        .split_whitespace();
    let method = parts.next().unwrap_or_default();
    let route = parts
        .next()
        .unwrap_or_default()
        .split('?')
        .next()
        .unwrap_or_default();
    if route == "/resolve" {
        if method != "POST" {
            return write_json_error(stream, "405 Method Not Allowed", "POST required");
        }
        return serve_resolve(stream, tracks, headers, &request[header_end + 4..]);
    }
    if method != "GET" && method != "HEAD" {
        return write_response(
            stream,
            "405 Method Not Allowed",
            "text/plain",
            b"GET or HEAD required",
            false,
        );
    }
    if route == "/catalog.json" {
        let manifest: Vec<Value> = tracks
            .read()
            .unwrap()
            .iter()
            .map(|track| {
                json!({
                    "id": track.id,
                    "title": track.title,
                    "artist": track.artist,
                    "duration": track.duration_seconds,
                    "url": track.source_url,
                })
            })
            .collect();
        let bytes = serde_json::to_vec(&manifest).unwrap_or_default();
        return write_response(
            stream,
            "200 OK",
            "application/json; charset=utf-8",
            &bytes,
            method == "HEAD",
        );
    }
    let asset = assets.read().unwrap().get(route).cloned();
    match asset {
        Some(asset) => serve_asset(stream, asset, headers, method == "HEAD", shutdown),
        None => write_response(
            stream,
            "404 Not Found",
            "text/plain",
            b"Audio not found",
            method == "HEAD",
        ),
    }
}

fn serve_asset(
    stream: &mut TcpStream,
    asset: Asset,
    headers: &str,
    head: bool,
    shutdown: &AtomicBool,
) -> std::io::Result<()> {
    let mut range = None;
    let mut if_range = None;
    for line in headers.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            return write_response(
                stream,
                "400 Bad Request",
                "text/plain",
                b"Malformed request header",
                head,
            );
        };
        let slot = if name.trim().eq_ignore_ascii_case("range") {
            Some(&mut range)
        } else if name.trim().eq_ignore_ascii_case("if-range") {
            Some(&mut if_range)
        } else {
            None
        };
        if let Some(slot) = slot
            && slot.replace(value.trim()).is_some()
        {
            return write_response(
                stream,
                "400 Bad Request",
                "text/plain",
                b"Duplicate range header",
                head,
            );
        }
    }
    match asset.data {
        AssetData::Synthetic(audio) => {
            let total = audio.len();
            let etag = format!("\"memory-{total}\"");
            let selected = range.filter(|_| if_range.is_none_or(|value| value == etag));
            let Some((start, count, partial)) = asset_range(stream, total, selected)? else {
                return Ok(());
            };
            write_asset_headers(stream, asset.mime, total, start, count, partial, &etag)?;
            if !head {
                stream.write_all(&audio.bytes()[start as usize..(start + count) as usize])?;
            }
            stream.flush()
        }
        AssetData::File {
            path,
            size,
            modified: expected_modified,
            authorization,
        } => {
            let opened = match authorization {
                Some(authorization) => open_authorized_request(&authorization)
                    .map(CatalogFile::Authorized)
                    .map_err(std::io::Error::other),
                None => std::fs::File::open(&path).map(CatalogFile::Selected),
            };
            let mut file = match opened {
                Ok(file) => file,
                Err(_) => {
                    return write_response(
                        stream,
                        "404 Not Found",
                        "text/plain",
                        b"Audio file unavailable",
                        head,
                    );
                }
            };
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return write_response(
                    stream,
                    "404 Not Found",
                    "text/plain",
                    b"Audio file unavailable",
                    head,
                );
            }
            if metadata.len() != size
                || system_time_ms(metadata.modified().ok()) != system_time_ms(expected_modified)
            {
                return write_response(
                    stream,
                    "409 Conflict",
                    "text/plain",
                    b"Audio file changed; rescan or import it again",
                    head,
                );
            }
            let total = metadata.len();
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |time| time.as_nanos());
            let etag = format!("\"file-{total}-{modified}\"");
            let selected = range.filter(|_| if_range.is_none_or(|value| value == etag));
            let Some((start, count, partial)) = asset_range(stream, total, selected)? else {
                return Ok(());
            };
            write_asset_headers(stream, asset.mime, total, start, count, partial, &etag)?;
            if !head {
                file.seek(SeekFrom::Start(start))?;
                let mut remaining = count;
                let mut chunk = [0_u8; 64 * 1024];
                while remaining > 0 {
                    if shutdown.load(Ordering::Acquire) {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "Catalog stopped",
                        ));
                    }
                    let limit = (remaining.min(chunk.len() as u64)) as usize;
                    let length = file.read(&mut chunk[..limit])?;
                    if length == 0 {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "Audio file changed during transfer",
                        ));
                    }
                    file.check()?;
                    stream.write_all(&chunk[..length])?;
                    remaining -= length as u64;
                }
            }
            stream.flush()
        }
    }
}

fn asset_range(
    stream: &mut TcpStream,
    total: u64,
    range: Option<&str>,
) -> std::io::Result<Option<(u64, u64, bool)>> {
    let Some(range) = range else {
        return Ok(Some((0, total, false)));
    };
    if let Some((start, end)) = parse_byte_range(range, total) {
        return Ok(Some((start, end - start + 1, true)));
    }
    write!(
        stream,
        "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */{total}\r\nAccept-Ranges: bytes\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;
    Ok(None)
}

fn parse_byte_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let value = value.strip_prefix("bytes=")?;
    if total == 0 || value.contains(',') {
        return None;
    }
    let (start, end) = value.trim().split_once('-')?;
    let parse = |value: &str| {
        if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
            value.parse::<u64>().ok()
        } else {
            None
        }
    };
    if start.is_empty() {
        let suffix = parse(end)?;
        if suffix == 0 {
            return None;
        }
        Some((total.saturating_sub(suffix), total - 1))
    } else {
        let start = parse(start)?;
        if start >= total {
            return None;
        }
        let end = if end.is_empty() {
            total - 1
        } else {
            parse(end)?.min(total - 1)
        };
        (end >= start).then_some((start, end))
    }
}

fn write_asset_headers(
    stream: &mut TcpStream,
    mime: &str,
    total: u64,
    start: u64,
    count: u64,
    partial: bool,
    etag: &str,
) -> std::io::Result<()> {
    let status = if partial {
        "206 Partial Content"
    } else {
        "200 OK"
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {count}\r\nAccept-Ranges: bytes\r\nETag: {etag}\r\nCache-Control: no-store\r\nConnection: close\r\n"
    )?;
    if partial {
        write!(
            stream,
            "Content-Range: bytes {}-{}/{total}\r\n",
            start,
            start + count - 1
        )?;
    }
    write!(stream, "\r\n")
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveBody {
    id: String,
    checksum: String,
    cipher: String,
    compressed: String,
}

fn serve_resolve(
    stream: &mut TcpStream,
    tracks: &RwLock<Vec<Track>>,
    headers: &str,
    initial_body: &[u8],
) -> std::io::Result<()> {
    let mut content_length = None;
    let mut content_type = None;
    let mut fixture_header = None;
    for line in headers.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            return write_json_error(stream, "400 Bad Request", "Malformed HTTP header");
        };
        let name = name.trim();
        let value = value.trim();
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return write_json_error(
                stream,
                "400 Bad Request",
                "Transfer-Encoding is unsupported",
            );
        }
        let slot = if name.eq_ignore_ascii_case("content-length") {
            Some(&mut content_length)
        } else if name.eq_ignore_ascii_case("content-type") {
            Some(&mut content_type)
        } else if name.eq_ignore_ascii_case("x-fixture") {
            Some(&mut fixture_header)
        } else {
            None
        };
        if let Some(slot) = slot
            && slot.replace(value).is_some()
        {
            return write_json_error(stream, "400 Bad Request", "Duplicate request header");
        }
    }
    let Some(content_length) = content_length else {
        return write_json_error(stream, "411 Length Required", "Content-Length required");
    };
    if content_length.is_empty() || !content_length.bytes().all(|byte| byte.is_ascii_digit()) {
        return write_json_error(stream, "400 Bad Request", "Invalid Content-Length");
    }
    let Ok(content_length) = content_length.parse::<usize>() else {
        return write_json_error(
            stream,
            "413 Content Too Large",
            "Resolve body exceeds 2 KiB",
        );
    };
    if content_length > MAX_RESOLVE_BODY_BYTES {
        return write_json_error(
            stream,
            "413 Content Too Large",
            "Resolve body exceeds 2 KiB",
        );
    }
    if initial_body.len() > content_length {
        return write_json_error(stream, "400 Bad Request", "Body exceeds Content-Length");
    }
    let body_deadline = Instant::now() + Duration::from_secs(2);
    let mut body = initial_body.to_vec();
    let mut chunk = [0_u8; 1024];
    while body.len() < content_length {
        let remaining = body_deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::TimedOut, "HTTP request body timed out")
            })?;
        stream.set_read_timeout(Some(remaining))?;
        let read_limit = chunk.len().min(content_length - body.len());
        let length = stream.read(&mut chunk[..read_limit])?;
        if length == 0 {
            return write_json_error(stream, "400 Bad Request", "Incomplete request body");
        }
        body.extend_from_slice(&chunk[..length]);
    }
    // Consume a bounded, correctly framed body before a semantic rejection, so
    // dropping a Windows socket with unread request data cannot truncate it.
    if content_type
        .and_then(|value| value.split(';').next())
        .is_none_or(|value| !value.trim().eq_ignore_ascii_case("application/json"))
    {
        return write_json_error(
            stream,
            "415 Unsupported Media Type",
            "application/json required",
        );
    }
    if fixture_header != Some(RESOLVE_FIXTURE_HEADER) {
        return write_json_error(
            stream,
            "400 Bad Request",
            "Original fixture header required",
        );
    }
    let body: ResolveBody = match serde_json::from_slice(&body) {
        Ok(body) => body,
        Err(_) => return write_json_error(stream, "400 Bad Request", "Invalid resolver JSON body"),
    };
    let expected_checksum = format!("{:x}", Md5::digest(body.id.as_bytes()));
    if body.checksum != expected_checksum {
        return write_json_error(
            stream,
            "422 Unprocessable Content",
            "ID checksum does not match MD5(id)",
        );
    }
    for encoded in [&body.cipher, &body.compressed] {
        if encoded.is_empty()
            || base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_or(true, |bytes| bytes.is_empty())
        {
            return write_json_error(
                stream,
                "400 Bad Request",
                "cipher/compressed must be nonempty base64",
            );
        }
    }
    let track = tracks
        .read()
        .unwrap()
        .iter()
        .find(|track| track.id == body.id && track.is_demo)
        .cloned();
    let Some(track) = track else {
        return write_json_error(stream, "404 Not Found", "Original tone ID not found");
    };
    // This endpoint validates transport fields and the MD5 fixture checksum.
    // It deliberately does not decrypt AES or decompress user-provided payloads.
    let response = serde_json::to_vec(&json!({"id": track.id, "url": track.source_url})).unwrap();
    write_response(
        stream,
        "200 OK",
        "application/json; charset=utf-8",
        &response,
        false,
    )
}

fn write_json_error(stream: &mut TcpStream, status: &str, message: &str) -> std::io::Result<()> {
    let body = serde_json::to_vec(&json!({"error": message})).unwrap();
    write_response(
        stream,
        status,
        "application/json; charset=utf-8",
        &body,
        false,
    )
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    mime: &str,
    bytes: &[u8],
    head: bool,
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        bytes.len()
    )?;
    if !head {
        stream.write_all(bytes)?;
    }
    stream.flush()
}

/// Original, deterministic PCM audio generated from oscillators; no third-party samples.
pub(crate) fn synthesized_wav(frequency: f64, duration_seconds: f64) -> Vec<u8> {
    let sample_count = (duration_seconds * SAMPLE_RATE as f64) as u32;
    let data_size = sample_count * 2;
    let mut bytes = Vec::with_capacity(44 + data_size as usize);
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_size).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    bytes.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_size.to_le_bytes());
    for sample in 0..sample_count {
        let time = sample as f64 / SAMPLE_RATE as f64;
        let fade = (time / 0.5).min(1.0) * ((duration_seconds - time) / 0.8).min(1.0);
        let pulse = 0.65 + 0.35 * (TAU * 0.25 * time).sin().powi(2);
        let chord = (TAU * frequency * time).sin() * 0.55
            + (TAU * frequency * 1.25 * time).sin() * 0.25
            + (TAU * frequency * 1.5 * time).sin() * 0.20;
        let sample = (chord * fade * pulse * 0.18 * i16::MAX as f64) as i16;
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_head_keeps_payloads_lazy_and_range_reads_match_original_audio() {
        let catalog = Catalog::new().unwrap();
        let tracks = catalog.tracks();
        let track = tracks.iter().find(|track| track.id == "tone-1").unwrap();
        let cached_bytes = || {
            catalog
                .inner
                .assets
                .read()
                .unwrap()
                .values()
                .map(|asset| match &asset.data {
                    AssetData::Synthetic(audio) => audio.bytes.get().map_or(0, Vec::len),
                    AssetData::File { .. } => 0,
                })
                .sum::<usize>()
        };
        assert_eq!(cached_bytes(), 0);
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let head = client.head(&track.source_url).send().unwrap();
        assert_eq!(head.status(), reqwest::StatusCode::OK);
        let length = head.headers()[reqwest::header::CONTENT_LENGTH]
            .to_str()
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert_eq!(cached_bytes(), 0);
        let response = client
            .get(&track.source_url)
            .header(reqwest::header::RANGE, "bytes=12-1537")
            .send()
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::PARTIAL_CONTENT);
        let expected = synthesized_wav(261.63, 45.0);
        assert_eq!(length, expected.len());
        assert_eq!(response.bytes().unwrap().as_ref(), &expected[12..1538]);
        assert_eq!(cached_bytes(), expected.len());
    }

    fn resolver_payload(id: &str) -> Value {
        // Endpoint tests use transport markers. The authored JS fixture separately
        // produces real AES/zlib bytes and verifies its own compression round-trip.
        json!({
            "id": id,
            "checksum": format!("{:x}", Md5::digest(id.as_bytes())),
            "cipher": base64::engine::general_purpose::STANDARD.encode([0_u8; 16]),
            "compressed": base64::engine::general_purpose::STANDARD.encode(b"original test marker"),
        })
    }

    fn read_raw_response(stream: &mut TcpStream) -> (String, Vec<u8>) {
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let boundary = response
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        (
            std::str::from_utf8(&response[..boundary])
                .unwrap()
                .to_string(),
            response[boundary + 4..].to_vec(),
        )
    }

    #[test]
    fn resolver_post_reads_fragmented_json_and_returns_original_audio_url() {
        let catalog = Catalog::new().unwrap();
        let address = catalog.base_url().strip_prefix("http://").unwrap();
        let body = serde_json::to_vec(&resolver_payload("tone-2")).unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let headers = format!(
            "POST /resolve HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json; charset=utf-8\r\nX-Fixture: {RESOLVE_FIXTURE_HEADER}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).unwrap();
        thread::sleep(Duration::from_millis(20));
        let midpoint = body.len() / 2;
        stream.write_all(&body[..midpoint]).unwrap();
        thread::sleep(Duration::from_millis(30));
        stream.write_all(&body[midpoint..]).unwrap();
        let (headers, body) = read_raw_response(&mut stream);
        assert!(headers.starts_with("HTTP/1.1 200 OK"));
        assert!(headers.contains(&format!("Content-Length: {}", body.len())));
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["id"], "tone-2");
        assert_eq!(response["url"], catalog.tracks()[1].source_url);
    }

    #[test]
    fn resolver_post_rejects_bad_json_digest_encoding_unknown_id_and_media_type() {
        let catalog = Catalog::new().unwrap();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .unwrap();
        let post = |body: String, media_type: &str| {
            client
                .post(format!("{}/resolve", catalog.base_url()))
                .header("Content-Type", media_type)
                .header("X-Fixture", RESOLVE_FIXTURE_HEADER)
                .body(body)
                .send()
                .unwrap()
        };
        assert_eq!(post("{\"id\":".into(), "application/json").status(), 400);
        let mut wrong_digest = resolver_payload("tone-1");
        wrong_digest["checksum"] = json!("00000000000000000000000000000000");
        assert_eq!(
            post(wrong_digest.to_string(), "application/json").status(),
            422
        );
        assert_eq!(
            post(resolver_payload("tone-4").to_string(), "application/json").status(),
            404
        );
        let mut empty_cipher = resolver_payload("tone-1");
        empty_cipher["cipher"] = json!("");
        assert_eq!(
            post(empty_cipher.to_string(), "application/json").status(),
            400
        );
        let mut bad_encoding = resolver_payload("tone-1");
        bad_encoding["compressed"] = json!("not base64!");
        assert_eq!(
            post(bad_encoding.to_string(), "application/json").status(),
            400
        );
        assert_eq!(
            post(resolver_payload("tone-1").to_string(), "text/plain").status(),
            415
        );
        assert_eq!(
            client
                .post(format!("{}/resolve", catalog.base_url()))
                .header("Content-Type", "application/json")
                .body(resolver_payload("tone-1").to_string())
                .send()
                .unwrap()
                .status(),
            400
        );
    }

    #[test]
    fn resolver_post_enforces_body_size_and_content_length_before_allocation() {
        let catalog = Catalog::new().unwrap();
        let address = catalog.base_url().strip_prefix("http://").unwrap();
        for (extra_headers, expected) in [
            (
                format!("Content-Length: {}\r\n", MAX_RESOLVE_BODY_BYTES + 1),
                "413",
            ),
            ("Content-Length: 10\r\nContent-Length: 10\r\n".into(), "400"),
            ("Content-Length: nope\r\n".into(), "400"),
            ("Transfer-Encoding: chunked\r\n".into(), "400"),
            (String::new(), "411"),
        ] {
            let mut stream = TcpStream::connect(address).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let headers = format!(
                "POST /resolve HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nX-Fixture: {RESOLVE_FIXTURE_HEADER}\r\n{extra_headers}Connection: close\r\n\r\n"
            );
            stream.write_all(headers.as_bytes()).unwrap();
            // These declarations must be rejected immediately without waiting
            // for, allocating, or retrying a potentially oversized body.
            let (headers, body) = read_raw_response(&mut stream);
            assert!(
                headers.starts_with(&format!("HTTP/1.1 {expected} ")),
                "unexpected response: {headers}"
            );
            assert!(serde_json::from_slice::<Value>(&body).unwrap()["error"].is_string());
        }
    }

    #[test]
    fn legal_synthesized_audio_is_decodable() {
        // A two-second excerpt includes a steady section beyond the 0.5s fade-in
        // and before the 0.8s fade-out. The first 1000 samples cover only 45ms.
        let decoder = Decoder::try_from(Cursor::new(synthesized_wav(220.0, 2.0))).unwrap();
        let duration = decoder.total_duration().unwrap().as_secs_f64();
        assert!((duration - 2.0).abs() < 0.01);
        let middle: Vec<f32> = decoder
            .skip((SAMPLE_RATE as f64 * 0.75) as usize)
            .take((SAMPLE_RATE as f64 * 0.5) as usize)
            .collect();
        assert_eq!(middle.len(), (SAMPLE_RATE as f64 * 0.5) as usize);
        let rms = (middle
            .iter()
            .map(|sample| (*sample as f64).powi(2))
            .sum::<f64>()
            / middle.len() as f64)
            .sqrt();
        let peak = middle
            .iter()
            .map(|sample| sample.abs())
            .fold(0.0_f32, f32::max);
        assert!(rms > 0.04, "steady-section RMS must be audible, got {rms}");
        assert!(
            peak > 0.08 && peak < 0.19,
            "steady-section peak must be audible and unclipped, got {peak}"
        );
    }

    #[test]
    fn loopback_catalog_serves_audio_manifest_and_search() {
        let catalog = Catalog::new().unwrap();
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(5))
            .no_proxy()
            .build()
            .unwrap();
        assert!(catalog.base_url().starts_with("http://127.0.0.1:"));
        let manifest: Vec<Value> = serde_json::from_slice(
            &client
                .get(catalog.manifest_url())
                .send()
                .unwrap()
                .bytes()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.len(), 3);
        assert_eq!(manifest[0]["id"], "tone-1");
        let track = catalog.search("  FIRST  ").remove(0);
        let bytes = client
            .get(&track.source_url)
            .send()
            .unwrap()
            .bytes()
            .unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert!(catalog.search("不存在的歌").is_empty());
        let legacy = track.legacy_music_info();
        for key in [
            "name",
            "singer",
            "source",
            "songmid",
            "interval",
            "albumName",
            "img",
            "typeUrl",
            "albumId",
            "types",
            "_types",
            "filePath",
            "ext",
        ] {
            assert!(legacy.get(key).is_some(), "missing {key}");
        }
        assert_eq!(legacy["source"], "local");
        assert!(track.request_info()["type"].is_null());
        assert_eq!(
            client
                .get(format!("{}/../private", catalog.base_url()))
                .send()
                .unwrap()
                .status(),
            404
        );
    }

    #[test]
    fn loopback_server_waits_for_delayed_fragmented_headers_and_sends_complete_wav() {
        let catalog = Catalog::new().unwrap();
        let address = catalog.base_url().strip_prefix("http://").unwrap();
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        // Accept occurs while no data is available, then the header terminator
        // arrives in another packet. Both reads must wait instead of aborting.
        thread::sleep(Duration::from_millis(100));
        stream
            .write_all(b"GET /tone-1.wav HTTP/1.1\r\nHost: localhost\r\n")
            .unwrap();
        thread::sleep(Duration::from_millis(40));
        stream.write_all(b"Connection: close\r\n\r\n").unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).unwrap();
        let boundary = response
            .windows(4)
            .position(|bytes| bytes == b"\r\n\r\n")
            .unwrap();
        let headers = std::str::from_utf8(&response[..boundary]).unwrap();
        assert!(headers.starts_with("HTTP/1.1 200 OK"));
        let expected_bytes = 44 + SAMPLE_RATE as usize * 45 * 2;
        assert!(headers.contains(&format!("Content-Length: {expected_bytes}")));
        let body = &response[boundary + 4..];
        assert_eq!(
            body.len(),
            expected_bytes,
            "the complete WAV body must arrive before EOF"
        );
        assert_eq!(&body[..4], b"RIFF");
        let decoded = Decoder::try_from(Cursor::new(body.to_vec())).unwrap();
        assert!((decoded.total_duration().unwrap().as_secs_f64() - 45.0).abs() < 0.02);
    }

    #[test]
    fn prepared_stable_identity_keeps_loopback_route_and_rejects_other_file_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("stable.wav");
        let body = synthesized_wav(997.0, 0.4);
        std::fs::write(&path, &body).unwrap();
        let catalog = Catalog::new().unwrap();
        let mut prepared = catalog.prepare_local(&path).unwrap();
        let mut identity = prepared.library_asset();
        identity.asset.id = "asset-v2:fixture".into();
        identity.asset.track_id = "local-v2:fixture".into();
        identity.track.id = "local-v2:fixture".into();
        identity.track.lx_id = "stable-fixture-music-id".into();
        identity.track.local_asset_id = Some(identity.asset.id.clone());
        let mut wrong = identity.clone();
        wrong.asset.size_bytes += 1;
        assert!(prepared.bind_library_identity(&wrong).is_err());
        prepared.bind_library_identity(&identity).unwrap();
        let current = prepared.library_asset();
        assert_eq!(current.asset.id, identity.asset.id);
        assert_eq!(current.track.id, identity.track.id);
        assert_eq!(current.track.lx_id, identity.track.lx_id);
        assert_eq!(current.file_identity, identity.file_identity);
        let published = catalog.commit_local(prepared);
        assert_eq!(published.id, identity.track.id);
        assert!(published.source_url.ends_with("/local-v2:fixture.wav"));
        let fetched = reqwest::blocking::get(&published.source_url).unwrap();
        assert_eq!(fetched.status(), reqwest::StatusCode::OK);
        assert_eq!(fetched.bytes().unwrap().as_ref(), body.as_slice());
    }

    #[test]
    fn local_import_validates_before_adding_track() {
        let catalog = Catalog::new().unwrap();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("lx-native-fixture-{unique}.wav"));
        std::fs::write(&path, b"corrupt file").unwrap();
        assert!(catalog.import_local(path.clone()).is_err());
        assert_eq!(catalog.tracks().len(), 3);
        std::fs::write(&path, synthesized_wav(330.0, 0.4)).unwrap();
        let track = catalog.import_local(path.clone()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(!track.is_demo);
        assert_eq!(catalog.tracks().len(), 4);
        assert!((track.duration_seconds - 0.4).abs() < 0.01);
        assert!(
            !track.legacy_music_info()["filePath"]
                .as_str()
                .unwrap()
                .contains("Users")
        );
    }
}
