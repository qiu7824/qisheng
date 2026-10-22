use crate::domain::lyric::LyricBundle;
use anyhow::{Context, Result, bail, ensure};
use lofty::config::{GlobalOptions, ParseOptions, ParsingMode, apply_global_options};
use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::{ItemKey, Tag};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const MAX_METADATA_READ_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PICTURE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TOTAL_PICTURE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PICTURES: usize = 16;

#[derive(Clone, Debug)]
pub struct LocalPicture {
    pub mime_type: String,
    pub picture_type: String,
    pub data: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct LocalMetadata {
    pub path: PathBuf,
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
    pub pictures: Vec<LocalPicture>,
    pub embedded_lyrics: Option<LyricBundle>,
    pub warnings: Vec<String>,
}

/// Reads tags and artwork from one selected file; never enumerates its directory.
pub fn read_local_metadata(path: impl AsRef<Path>) -> Result<LocalMetadata> {
    let path = std::fs::canonicalize(path.as_ref()).context("无法定位所选音频文件")?;
    let file = File::open(&path).context("无法读取所选音频文件")?;
    ensure!(file.metadata()?.is_file(), "所选音频不是普通文件");
    let length = file.metadata()?.len();
    read_local_metadata_from_open_file(file, path, length)
}

/// Parses one already-opened, independently positioned reader. Authorized
/// readers retain their cancellation and grant checks for every bounded read.
pub fn read_local_metadata_from_open_file<R: Read + Seek>(
    mut file: R,
    path: PathBuf,
    length: u64,
) -> Result<LocalMetadata> {
    ensure!(
        path.is_absolute() && path.to_str().is_some(),
        "音频元数据路径无效"
    );
    preflight_id3(&mut file, length)?;
    file.seek(SeekFrom::Start(0))?;
    // Lofty's configuration is thread-local. Apply limits in each scanning worker.
    apply_global_options(
        GlobalOptions::new()
            .allocation_limit(MAX_PICTURE_BYTES)
            .use_custom_resolvers(false)
            .preserve_format_specific_items(false),
    );
    let reader = MetadataReader {
        file,
        remaining: MAX_METADATA_READ_BYTES,
    };
    let probe = Probe::new(reader).guess_file_type()?;
    ensure!(
        matches!(
            probe.file_type(),
            Some(
                FileType::Aac
                    | FileType::Flac
                    | FileType::Mpeg
                    | FileType::Mp4
                    | FileType::Opus
                    | FileType::Vorbis
                    | FileType::Wav
            )
        ),
        "UnsupportedCapability: 音频标签格式不受支持"
    );
    let tagged = probe
        .options(
            ParseOptions::new()
                .parsing_mode(ParsingMode::Strict)
                .max_junk_bytes(4096),
        )
        .read()
        .context("音频标签损坏或格式不受支持")?;
    let mut result = LocalMetadata {
        path,
        title: None,
        artists: Vec::new(),
        album: None,
        duration_ms: tagged
            .properties()
            .duration()
            .as_millis()
            .min(u64::MAX as u128) as u64,
        pictures: Vec::new(),
        embedded_lyrics: None,
        warnings: Vec::new(),
    };
    let mut tags = Vec::new();
    if let Some(primary) = tagged.primary_tag() {
        tags.push(primary);
    }
    for tag in tagged.tags() {
        if !tags.iter().any(|previous| std::ptr::eq(*previous, tag)) {
            tags.push(tag);
        }
    }
    let mut picture_bytes = 0usize;
    for tag in tags {
        ensure!(tag.items().len() <= 4096, "音频标签条目超过4096项限制");
        if result.title.is_none() {
            result.title = text_field(tag, ItemKey::TrackTitle)?;
        }
        if result.album.is_none() {
            result.album = text_field(tag, ItemKey::AlbumTitle)?;
        }
        if result.artists.is_empty() {
            let preferred: Vec<_> = tag.get_strings(ItemKey::TrackArtists).collect();
            let artists = if preferred.is_empty() {
                tag.get_strings(ItemKey::TrackArtist).collect()
            } else {
                preferred
            };
            for artist in artists {
                let artist = artist.trim();
                ensure!(
                    artist.len() <= 8192 && !artist.contains('\0'),
                    "歌手标签内容无效或超限"
                );
                if !artist.is_empty() && !result.artists.iter().any(|value| value == artist) {
                    ensure!(result.artists.len() < 128, "歌手标签超过128项限制");
                    result.artists.push(artist.to_owned());
                }
            }
        }
        if result.embedded_lyrics.is_none()
            && let Some(text) = tag
                .get_string(ItemKey::Lyrics)
                .or_else(|| tag.get_string(ItemKey::UnsyncLyrics))
        {
            match crate::lyrics::local::decode_lyric_bundle(text) {
                Ok(bundle) => result.embedded_lyrics = Some(bundle),
                Err(error) => result.warnings.push(format!("内嵌歌词无法读取：{error}")),
            }
        }
        for picture in tag.pictures() {
            let data = picture.data();
            ensure!(data.len() <= MAX_PICTURE_BYTES, "单张封面超过8 MiB限制");
            picture_bytes = picture_bytes
                .checked_add(data.len())
                .context("封面大小溢出")?;
            ensure!(
                picture_bytes <= MAX_TOTAL_PICTURE_BYTES && result.pictures.len() < MAX_PICTURES,
                "封面总量或数量超过限制"
            );
            if let Some(mime) = picture_mime(data) {
                result.pictures.push(LocalPicture {
                    mime_type: mime.into(),
                    picture_type: format!("{:?}", picture.pic_type()),
                    data: data.to_vec(),
                });
            } else {
                result
                    .warnings
                    .push("封面图片格式不受支持或文件头无效".into());
            }
        }
    }
    Ok(result)
}

/// Lofty's compressed ID3 text path allocates after decompression. Until that
/// path supports an output budget, reject compressed frames before parsing.
fn preflight_id3(file: &mut (impl Read + Seek), length: u64) -> Result<()> {
    let mut offset = 0_u64;
    let mut read_bytes = 0usize;
    let mut padding = [0_u8; 4096];
    file.seek(SeekFrom::Start(0))?;
    let inspected = file.read(&mut padding)?;
    if let Some(first) = padding[..inspected].iter().position(|byte| *byte != 0) {
        offset = first as u64;
    } else if inspected == padding.len() {
        bail!("音频起始填充超过4096字节限制");
    }
    for tag_index in 0..=128 {
        if length.saturating_sub(offset) < 10 {
            break;
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0_u8; 10];
        file.read_exact(&mut header)?;
        if &header[..3] != b"ID3" {
            break;
        }
        ensure!(tag_index < 128, "ID3标签数量超过128项限制");
        let size = syncsafe(&header[6..10])?;
        ensure!(size <= MAX_PICTURE_BYTES, "ID3标签超过8 MiB限制");
        read_bytes = read_bytes
            .checked_add(size + 10)
            .context("ID3标签长度溢出")?;
        ensure!(
            read_bytes <= MAX_METADATA_READ_BYTES,
            "ID3标签总量超过16 MiB限制"
        );
        ensure!(offset + 10 + size as u64 <= length, "ID3标签被截断");
        let mut body = vec![0; size];
        file.read_exact(&mut body)?;
        check_id3_frames(&header, body)?;
        offset += 10 + size as u64;
        if header[3] == 4 && header[5] & 0x10 != 0 {
            offset += 10;
        }
    }
    file.seek(SeekFrom::Start(offset))?;
    let mut riff = [0_u8; 12];
    if file.read(&mut riff)? != 12 || &riff[..4] != b"RIFF" || &riff[8..] != b"WAVE" {
        return Ok(());
    }
    // Lofty's WAV reader walks the actual file length, including trailing chunks.
    let riff_end = length;
    offset += 12;
    for _ in 0..65_536 {
        if riff_end.saturating_sub(offset) < 8 {
            return Ok(());
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut chunk = [0_u8; 8];
        file.read_exact(&mut chunk)?;
        let size = u32::from_le_bytes(chunk[4..].try_into().unwrap()) as usize;
        let next = offset
            .checked_add(8 + size as u64 + (size as u64 & 1))
            .context("WAV块长度溢出")?;
        ensure!(next <= length + 1, "WAV标签块被截断");
        if chunk[..4].eq_ignore_ascii_case(b"id3 ") {
            ensure!(size <= MAX_PICTURE_BYTES, "WAV内嵌ID3超过8 MiB限制");
            read_bytes = read_bytes.checked_add(size).context("WAV标签长度溢出")?;
            ensure!(
                read_bytes <= MAX_METADATA_READ_BYTES,
                "WAV标签总量超过16 MiB限制"
            );
            let mut data = vec![0; size];
            file.read_exact(&mut data)?;
            ensure!(data.len() >= 10 && &data[..3] == b"ID3", "WAV内嵌ID3头无效");
            let header: [u8; 10] = data[..10].try_into().unwrap();
            let body_size = syncsafe(&header[6..])?;
            ensure!(body_size <= data.len() - 10, "WAV内嵌ID3被截断");
            check_id3_frames(&header, data[10..10 + body_size].to_vec())?;
        }
        offset = next;
    }
    bail!("WAV块数量超过限制")
}

fn syncsafe(bytes: &[u8]) -> Result<usize> {
    ensure!(
        bytes.len() == 4 && bytes.iter().all(|byte| byte & 0x80 == 0),
        "ID3同步安全整数无效"
    );
    Ok(bytes
        .iter()
        .fold(0usize, |value, byte| (value << 7) | *byte as usize))
}

fn check_id3_frames(header: &[u8; 10], mut body: Vec<u8>) -> Result<()> {
    let version = header[3];
    ensure!(
        (2..=4).contains(&version),
        "UnsupportedCapability: ID3版本不受支持"
    );
    if version == 2 && header[5] & 0x40 != 0 {
        bail!("UnsupportedCapability: 压缩ID3标签暂不支持");
    }
    if header[5] & 0x80 != 0 && version < 4 {
        let mut filtered = Vec::with_capacity(body.len());
        let mut offset = 0;
        while offset < body.len() {
            let byte = body[offset];
            filtered.push(byte);
            offset += 1;
            if byte == 0xff && body.get(offset) == Some(&0) {
                offset += 1;
            }
        }
        body = filtered;
    }
    let mut offset = 0usize;
    if version >= 3 && header[5] & 0x40 != 0 {
        ensure!(body.len() >= 4, "ID3扩展头被截断");
        offset = if version == 4 {
            syncsafe(&body[..4])?
        } else {
            4 + u32::from_be_bytes(body[..4].try_into().unwrap()) as usize
        };
        ensure!(offset >= 4 && offset <= body.len(), "ID3扩展头长度无效");
    }
    let frame_header = if version == 2 { 6 } else { 10 };
    let mut frames = 0;
    while body.len().saturating_sub(offset) >= frame_header {
        if body[offset] == 0 {
            break;
        }
        frames += 1;
        ensure!(frames <= 4096, "ID3帧数量超过4096项限制");
        let frame = &body[offset..offset + frame_header];
        let size = if version == 2 {
            ((frame[3] as usize) << 16) | ((frame[4] as usize) << 8) | frame[5] as usize
        } else if version == 4 {
            syncsafe(&frame[4..8])?
        } else {
            u32::from_be_bytes(frame[4..8].try_into().unwrap()) as usize
        };
        if version >= 3 {
            let compression = if version == 4 { 0x08 } else { 0x80 };
            ensure!(
                frame[9] & compression == 0,
                "UnsupportedCapability: 压缩ID3帧暂不支持"
            );
        }
        let next = offset
            .checked_add(frame_header)
            .and_then(|offset| offset.checked_add(size))
            .context("ID3帧长度溢出")?;
        ensure!(next <= body.len(), "ID3帧被截断");
        offset = next;
    }
    Ok(())
}

fn text_field(tag: &Tag, key: ItemKey) -> Result<Option<String>> {
    let Some(text) = tag.get_string(key) else {
        return Ok(None);
    };
    ensure!(
        text.len() <= 8192 && !text.contains('\0'),
        "音乐文本标签内容无效或超过8 KiB限制"
    );
    let text = text.trim();
    Ok((!text.is_empty()).then(|| text.to_owned()))
}

fn picture_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else if bytes.len() >= 14 && bytes.starts_with(b"BM") {
        Some("image/bmp")
    } else {
        None
    }
}

struct MetadataReader<R> {
    file: R,
    remaining: usize,
}

impl<R: Read> Read for MetadataReader<R> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() {
            return Ok(0);
        }
        if self.remaining == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Metadata read budget exceeded",
            ));
        }
        let limit = output.len().min(self.remaining);
        let length = self.file.read(&mut output[..limit])?;
        self.remaining -= length;
        Ok(length)
    }
}

impl<R: Seek> Seek for MetadataReader<R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.file.seek(position)
    }
}
