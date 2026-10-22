//! Verified MP3/FLAC tag mutation of an independently authorized editable copy.
use super::{
    artwork_validator::{VerifiedArtwork, validate_artwork},
    scan::ScanContext,
};
use anyhow::{Context, Result, ensure};
use lofty::{
    TextEncoding,
    config::{GlobalOptions, ParseOptions, ParsingMode, WriteOptions, apply_global_options},
    file::{AudioFile, FileType, TaggedFile, TaggedFileExt},
    flac::FlacFile,
    id3::v2::{Frame, Id3v2Tag, UnsynchronizedTextFrame},
    io::{Length, Truncate},
    ogg::{OggPictureStorage, tag::VorbisComments},
    picture::{Picture, PictureType},
    probe::Probe,
    tag::{ItemKey, ItemValue, Tag, TagExt, TagItem, TagType},
};
use rodio::Decoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};

pub const MAX_TAG_METADATA_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_TAG_COVER_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_TAG_FILE_BYTES: u64 = 1024 * 1024 * 1024;
const IO_BYTES: usize = 64 * 1024;

/// The adapter must own mutation rights to an editable copy. A Library
/// AuthorizedFile intentionally does not implement this trait or Write.
pub trait EditableTagFile: Read + Write + Seek + Length + Truncate {
    fn check_write_authority(&self) -> io::Result<()>;
    fn sync_data(&mut self) -> io::Result<()>;
}
#[derive(Clone, Debug, Default)]
pub enum TagField<T> {
    #[default]
    Keep,
    Set(T),
    Remove,
}
#[derive(Clone, Debug)]
pub struct VerifiedTagCover {
    artwork: VerifiedArtwork,
}
impl VerifiedTagCover {
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        Self::from_bytes_checked(bytes, || Ok(()))
    }
    pub fn from_bytes_checked(bytes: Vec<u8>, check: impl Fn() -> Result<()>) -> Result<Self> {
        ensure!(
            bytes.len() <= MAX_TAG_COVER_BYTES,
            "Tag cover exceeds 8 MiB"
        );
        let format = image::guess_format(&bytes)?;
        ensure!(
            matches!(format, image::ImageFormat::Jpeg | image::ImageFormat::Png),
            "Tag cover must be JPEG or PNG"
        );
        Ok(Self {
            artwork: validate_artwork(bytes, None, check)?,
        })
    }
    pub fn encoded(&self) -> &[u8] {
        self.artwork.encoded()
    }
    pub fn mime_type(&self) -> &str {
        &self.artwork.metadata().mime_type
    }
    fn picture(&self) -> Result<Picture> {
        let mut picture = Picture::from_reader(&mut self.encoded())?;
        picture.set_pic_type(PictureType::CoverFront);
        picture.set_description(None);
        Ok(picture)
    }
}
#[derive(Clone, Debug, Default)]
pub struct LocalTagPatch {
    pub title: TagField<String>,
    pub artists: TagField<Vec<String>>,
    pub album: TagField<String>,
    pub lyrics: TagField<String>,
    pub cover: TagField<VerifiedTagCover>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TagWriteFormat {
    Mp3,
    Flac,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TagMetadataSnapshot {
    pub format: TagWriteFormat,
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub lyrics: Option<String>,
}

/// Re-reads the actual admitted handle without granting another read or writer.
pub fn read_local_tag_metadata<F: EditableTagFile>(
    file: &mut F,
    context: &ScanContext,
) -> Result<TagMetadataSnapshot> {
    context.check()?;
    file.check_write_authority()?;
    apply_global_options(
        GlobalOptions::new()
            .allocation_limit(MAX_TAG_COVER_BYTES)
            .use_custom_resolvers(false)
            .preserve_format_specific_items(true),
    );
    let mut io = CheckedIo {
        file,
        context,
        read_remaining: None,
    };
    let tagged = read_tags(&mut io)?;
    let kind = match tagged.file_type() {
        FileType::Mpeg => TagType::Id3v2,
        FileType::Flac => TagType::VorbisComments,
        _ => anyhow::bail!("Tag metadata evidence only supports MP3 and FLAC"),
    };
    let empty = Tag::new(kind);
    let tag = tagged.tag(kind).unwrap_or(&empty);
    let title = tag.get_string(ItemKey::TrackTitle).map(str::to_owned);
    let artists = tag
        .get_strings(ItemKey::TrackArtist)
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let album = tag.get_string(ItemKey::AlbumTitle).map(str::to_owned);
    let lyrics = if kind == TagType::Id3v2 {
        let id3: Id3v2Tag = tag.clone().into();
        id3.unsync_text()
            .find(|value| value.language == *b"zho" && value.description.is_empty())
            .map(|value| value.content.clone().into_owned())
    } else {
        tag.get_string(ItemKey::Lyrics).map(str::to_owned)
    };
    let metadata = TagMetadataSnapshot {
        format: if kind == TagType::Id3v2 {
            TagWriteFormat::Mp3
        } else {
            TagWriteFormat::Flac
        },
        title,
        artists,
        album,
        lyrics,
    };
    ensure!(
        metadata.artists.len() <= 64,
        "Observed artists exceed 64 entries"
    );
    let texts = metadata
        .title
        .iter()
        .chain(metadata.album.iter())
        .chain(metadata.lyrics.iter())
        .chain(metadata.artists.iter());
    let mut bytes = 0usize;
    for value in texts {
        ensure!(!value.contains('\0'), "Observed tag text contains NUL");
        bytes = bytes
            .checked_add(value.len())
            .context("Observed tag text size overflow")?;
        ensure!(bytes <= 1024 * 1024, "Observed tag text exceeds 1 MiB");
    }
    io.check()?;
    Ok(metadata)
}

pub(crate) fn read_local_tag_audio_sha256<F: EditableTagFile>(
    file: &mut F,
    context: &ScanContext,
) -> Result<String> {
    let mut io = CheckedIo {
        file,
        context,
        read_remaining: None,
    };
    let actual = layout(&mut io)?;
    let sha256 = digest_range(&mut io, actual.audio_start, actual.length)?;
    io.check()?;
    Ok(sha256)
}
#[derive(Clone, Debug)]
pub struct TagWriteVerification {
    pub format: TagWriteFormat,
    pub audio_sha256: String,
    pub audio_bytes: u64,
    pub body_sha256: String,
    pub size_bytes: u64,
}

/// This never replaces a user file or publishes a read grant. On failure the
/// caller discards the editable copy and retains its original sealed body.
pub fn write_local_tags<F: EditableTagFile>(
    file: &mut F,
    patch: &LocalTagPatch,
    context: &ScanContext,
) -> Result<TagWriteVerification> {
    context.check()?;
    file.check_write_authority()?;
    validate_patch(patch)?;
    let mut io = CheckedIo {
        file,
        context,
        read_remaining: None,
    };
    let before = layout(&mut io)?;
    let preserved_blocks =
        preserved_flac_blocks(&mut io, &before, !matches!(patch.cover, TagField::Keep))?;
    let audio_sha256 = digest_range(&mut io, before.audio_start, before.length)?;
    let audio_bytes = before.length - before.audio_start;
    apply_global_options(
        GlobalOptions::new()
            .allocation_limit(MAX_TAG_COVER_BYTES)
            .use_custom_resolvers(false)
            .preserve_format_specific_items(true),
    );
    let options = WriteOptions::new()
        .remove_others(false)
        .respect_read_only(true)
        .lossy_text_encoding(false)
        .parse_options(parse_options());
    if before.format == TagWriteFormat::Mp3 {
        let mut tagged = read_tags(&mut io)?;
        ensure!(
            tagged.file_type() == FileType::Mpeg,
            "Tag format does not match the actual editable body"
        );
        let mut tag = tagged
            .remove(TagType::Id3v2)
            .unwrap_or_else(|| Tag::new(TagType::Id3v2));
        patch_tag(&mut tag, patch)?;
        let mut id3: Id3v2Tag = tag.into();
        if !matches!(patch.lyrics, TagField::Keep) {
            id3.retain(|frame| {
                !matches!(frame,Frame::UnsynchronizedText(text)
                if text.language == *b"zho" && text.description.is_empty())
            });
            if let TagField::Set(text) = &patch.lyrics {
                id3.insert(Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
                    TextEncoding::UTF8,
                    *b"zho",
                    "",
                    text.clone(),
                )));
            }
        }
        id3.dump_to(&mut MetadataCounter::default(), options)?;
        io.seek(SeekFrom::Start(0))?;
        id3.save_to(&mut io, options)?;
    } else {
        write_flac_tags(&mut io, patch, options)?;
    }
    io.check()?;
    let after = layout(&mut io)?;
    ensure!(
        after.format == before.format && after.stream_info == before.stream_info,
        "Tag writing changed the encoded stream description"
    );
    ensure!(
        preserved_flac_blocks(&mut io, &after, !matches!(patch.cover, TagField::Keep))?
            == preserved_blocks,
        "Tag writing changed untouched format-specific metadata blocks"
    );
    ensure!(
        after.length - after.audio_start == audio_bytes
            && digest_range(&mut io, after.audio_start, after.length)? == audio_sha256,
        "Tag writing changed the original encoded audio payload"
    );
    verify_patch(&read_tags(&mut io)?, patch, after.format)?;
    verify_decode(&mut io, &after)?;
    let body_sha256 = digest_range(&mut io, 0, after.length)?;
    io.check()?;
    io.file.sync_data()?;
    io.check()?;
    Ok(TagWriteVerification {
        format: after.format,
        audio_sha256,
        audio_bytes,
        body_sha256,
        size_bytes: after.length,
    })
}

fn validate_patch(patch: &LocalTagPatch) -> Result<()> {
    let mut bytes = 0usize;
    for field in [&patch.title, &patch.album, &patch.lyrics] {
        if let TagField::Set(value) = field {
            ensure!(
                !value.is_empty() && !value.contains('\0'),
                "Tag text must be nonempty and contain no NUL"
            );
            bytes = bytes.saturating_add(value.len());
        }
    }
    if let TagField::Set(artists) = &patch.artists {
        ensure!(
            !artists.is_empty() && artists.len() <= 64,
            "Tag artists exceed their item limit"
        );
        for value in artists {
            ensure!(
                !value.is_empty() && !value.contains('\0'),
                "Tag artist must be nonempty and contain no NUL"
            );
            bytes = bytes.saturating_add(value.len());
        }
    }
    ensure!(bytes <= 1024 * 1024, "Tag text exceeds 1 MiB");
    Ok(())
}
fn patch_text(tag: &mut Tag, key: ItemKey, field: &TagField<String>) {
    match field {
        TagField::Keep => {}
        TagField::Remove => tag.remove_key(key),
        TagField::Set(value) => {
            tag.insert_text(key, value.clone());
        }
    }
}
fn patch_tag(tag: &mut Tag, patch: &LocalTagPatch) -> Result<()> {
    patch_text(tag, ItemKey::TrackTitle, &patch.title);
    patch_text(tag, ItemKey::AlbumTitle, &patch.album);
    if !matches!(patch.artists, TagField::Keep) {
        tag.remove_key(ItemKey::TrackArtist);
        if let TagField::Set(values) = &patch.artists {
            for value in values {
                ensure!(
                    tag.push(TagItem::new(
                        ItemKey::TrackArtist,
                        ItemValue::Text(value.clone())
                    )),
                    "Artist is not writable in this tag format"
                );
            }
        }
    }
    if tag.tag_type() == TagType::VorbisComments {
        patch_text(tag, ItemKey::Lyrics, &patch.lyrics);
    }
    if !matches!(patch.cover, TagField::Keep) {
        tag.remove_picture_type(PictureType::CoverFront);
        if let TagField::Set(cover) = &patch.cover {
            tag.push_picture(cover.picture()?);
        }
    }
    let picture_bytes = tag
        .pictures()
        .iter()
        .map(|picture| picture.data().len())
        .sum::<usize>();
    ensure!(
        picture_bytes <= MAX_TAG_METADATA_BYTES,
        "Combined tag artwork exceeds 16 MiB"
    );
    Ok(())
}
fn patch_vorbis_text(tag: &mut VorbisComments, key: &str, field: &TagField<String>) {
    if !matches!(field, TagField::Keep) {
        drop(tag.remove(key));
        if let TagField::Set(value) = field {
            tag.push(key.into(), value.clone());
        }
    }
}
fn write_flac_tags<F: EditableTagFile>(
    file: &mut CheckedIo<'_, F>,
    patch: &LocalTagPatch,
    options: WriteOptions,
) -> Result<()> {
    file.seek(SeekFrom::Start(0))?;
    file.read_remaining = Some(MAX_TAG_METADATA_BYTES as u64);
    let parsed = FlacFile::read_from(file, parse_options());
    file.read_remaining = None;
    let mut flac = parsed?;
    // The typed tag retains unrecognized comment keys and original vendor;
    // conversion through generic Tag would discard that format remainder.
    let mut comments = flac.vorbis_comments().cloned().unwrap_or_default();
    patch_vorbis_text(&mut comments, "TITLE", &patch.title);
    patch_vorbis_text(&mut comments, "ALBUM", &patch.album);
    patch_vorbis_text(&mut comments, "LYRICS", &patch.lyrics);
    if !matches!(patch.artists, TagField::Keep) {
        drop(comments.remove("ARTIST"));
        if let TagField::Set(values) = &patch.artists {
            for value in values {
                comments.push("ARTIST".into(), value.clone());
            }
        }
    }
    if !matches!(patch.cover, TagField::Keep) {
        comments.remove_picture_type(PictureType::CoverFront);
        flac.remove_picture_type(PictureType::CoverFront);
        if let TagField::Set(cover) = &patch.cover {
            flac.insert_picture(cover.picture()?, None)?;
        }
    }
    let picture_bytes = flac
        .pictures()
        .iter()
        .chain(comments.pictures().iter())
        .map(|(picture, _)| picture.data().len())
        .sum::<usize>();
    ensure!(
        picture_bytes <= MAX_TAG_METADATA_BYTES,
        "Combined tag artwork exceeds 16 MiB"
    );
    comments.dump_to(&mut MetadataCounter::default(), options)?;
    flac.set_vorbis_comments(comments);
    file.seek(SeekFrom::Start(0))?;
    flac.save_to(file, options)?;
    Ok(())
}
fn verify_text(tag: &Tag, key: ItemKey, field: &TagField<String>) -> Result<()> {
    match field {
        TagField::Keep => {}
        TagField::Remove => ensure!(
            tag.get_string(key).is_none(),
            "Removed tag field survived readback"
        ),
        TagField::Set(value) => ensure!(
            tag.get_string(key) == Some(value.as_str()),
            "Written tag text did not match readback"
        ),
    }
    Ok(())
}
fn verify_patch(tagged: &TaggedFile, patch: &LocalTagPatch, format: TagWriteFormat) -> Result<()> {
    let ty = if format == TagWriteFormat::Mp3 {
        TagType::Id3v2
    } else {
        TagType::VorbisComments
    };
    let empty = Tag::new(ty);
    let tag = tagged.tag(ty).unwrap_or(&empty);
    verify_text(tag, ItemKey::TrackTitle, &patch.title)?;
    verify_text(tag, ItemKey::AlbumTitle, &patch.album)?;
    if !matches!(patch.artists, TagField::Keep) {
        let actual = tag.get_strings(ItemKey::TrackArtist).collect::<Vec<_>>();
        match &patch.artists {
            TagField::Set(values) => ensure!(
                actual == values.iter().map(String::as_str).collect::<Vec<_>>(),
                "Written artist list did not match readback"
            ),
            TagField::Remove => ensure!(actual.is_empty(), "Removed artists survived readback"),
            TagField::Keep => {}
        }
    }
    if format == TagWriteFormat::Mp3 {
        let id3: Id3v2Tag = tag.clone().into();
        let mut selected = id3
            .unsync_text()
            .filter(|text| text.language == *b"zho" && text.description.is_empty());
        match &patch.lyrics {
            TagField::Set(value) => ensure!(
                selected
                    .next()
                    .is_some_and(|text| text.content == value.as_str())
                    && selected.next().is_none(),
                "Written zho lyrics did not match readback"
            ),
            TagField::Remove => ensure!(
                selected.next().is_none(),
                "Removed zho lyrics survived readback"
            ),
            TagField::Keep => {}
        }
    } else {
        verify_text(tag, ItemKey::Lyrics, &patch.lyrics)?;
    }
    let covers = tag
        .pictures()
        .iter()
        .filter(|picture| picture.pic_type() == PictureType::CoverFront)
        .collect::<Vec<_>>();
    match &patch.cover {
        TagField::Set(cover) => ensure!(
            covers.len() == 1 && covers[0].data() == cover.encoded(),
            "Written cover did not match actual readback bytes"
        ),
        TagField::Remove => ensure!(covers.is_empty(), "Removed front cover survived readback"),
        TagField::Keep => {}
    }
    Ok(())
}
fn parse_options() -> ParseOptions {
    ParseOptions::new()
        .parsing_mode(ParsingMode::Strict)
        .read_properties(false)
        .max_junk_bytes(4096)
}
fn read_tags<F: EditableTagFile>(file: &mut CheckedIo<'_, F>) -> Result<TaggedFile> {
    file.seek(SeekFrom::Start(0))?;
    file.read_remaining = Some(MAX_TAG_METADATA_BYTES as u64);
    let result = Probe::new(&mut *file)
        .guess_file_type()?
        .options(parse_options())
        .read();
    file.read_remaining = None;
    Ok(result?)
}
struct Layout {
    format: TagWriteFormat,
    audio_start: u64,
    length: u64,
    stream_info: Option<[u8; 34]>,
    flac_start: Option<u64>,
}
fn layout<F: EditableTagFile>(file: &mut CheckedIo<'_, F>) -> Result<Layout> {
    let length = Length::len(file)?;
    ensure!(
        length > 0 && length <= MAX_TAG_FILE_BYTES + MAX_TAG_METADATA_BYTES as u64,
        "Editable tag body exceeds its size limit"
    );
    file.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 10];
    file.read_exact(&mut head)?;
    let mut position = 0u64;
    if &head[..3] == b"ID3" {
        ensure!(
            head[6..10].iter().all(|byte| byte & 0x80 == 0),
            "Invalid ID3 tag size"
        );
        let size = head[6..10]
            .iter()
            .fold(0u64, |size, byte| (size << 7) | u64::from(*byte));
        position = 10
            + size
            + if head[3] == 4 && head[5] & 0x10 != 0 {
                10
            } else {
                0
            };
        ensure!(
            position <= MAX_TAG_METADATA_BYTES as u64 && position < length,
            "ID3 metadata exceeds 16 MiB or has no audio"
        );
        file.seek(SeekFrom::Start(position))?;
        file.read_exact(&mut head[..4])?;
    }
    if &head[..4] == b"fLaC" {
        let flac_start = position;
        position += 4;
        let mut stream_info = None;
        for index in 0..4096 {
            file.seek(SeekFrom::Start(position))?;
            let mut header = [0u8; 4];
            file.read_exact(&mut header)?;
            let size =
                (u64::from(header[1]) << 16) | (u64::from(header[2]) << 8) | u64::from(header[3]);
            ensure!(
                index != 0 || (header[0] & 0x7f == 0 && size == 34),
                "FLAC has no valid initial STREAMINFO"
            );
            if index == 0 {
                let mut data = [0u8; 34];
                file.read_exact(&mut data)?;
                stream_info = Some(data);
            }
            position = position
                .checked_add(4 + size)
                .context("FLAC metadata size overflow")?;
            ensure!(
                position <= MAX_TAG_METADATA_BYTES as u64 && position < length,
                "FLAC metadata exceeds 16 MiB or has no audio"
            );
            if header[0] & 0x80 != 0 {
                ensure!(
                    length - position <= MAX_TAG_FILE_BYTES,
                    "Encoded audio exceeds 1 GiB"
                );
                return Ok(Layout {
                    format: TagWriteFormat::Flac,
                    audio_start: position,
                    length,
                    stream_info,
                    flac_start: Some(flac_start),
                });
            }
        }
        anyhow::bail!("FLAC metadata exceeds its block limit")
    }
    ensure!(
        head[0] == 0xff
            && head[1] & 0xe0 == 0xe0
            && head[1] & 0x06 == 0x02
            && (head[1] >> 3) & 0x03 != 1,
        "UnsupportedCapability: tag writing supports only MP3 and FLAC"
    );
    ensure!(
        length - position <= MAX_TAG_FILE_BYTES,
        "Encoded audio exceeds 1 GiB"
    );
    Ok(Layout {
        format: TagWriteFormat::Mp3,
        audio_start: position,
        length,
        stream_info: None,
        flac_start: None,
    })
}
fn preserved_flac_blocks<F: EditableTagFile>(
    file: &mut CheckedIo<'_, F>,
    layout: &Layout,
    cover_changed: bool,
) -> Result<Vec<(u8, Vec<u8>)>> {
    let Some(start) = layout.flac_start else {
        return Ok(Vec::new());
    };
    let mut blocks = Vec::new();
    let mut position = start + 4;
    while position < layout.audio_start {
        file.seek(SeekFrom::Start(position))?;
        let mut header = [0u8; 4];
        file.read_exact(&mut header)?;
        let ty = header[0] & 0x7f;
        let length =
            (usize::from(header[1]) << 16) | (usize::from(header[2]) << 8) | usize::from(header[3]);
        ensure!(
            length <= MAX_TAG_METADATA_BYTES && position + 4 + length as u64 <= layout.audio_start,
            "Invalid FLAC metadata block"
        );
        if ty != 1 && ty != 4 {
            let mut body = vec![0; length];
            file.read_exact(&mut body)?;
            let replaced_front = ty == 6
                && cover_changed
                && body.get(..4).is_some_and(|kind| kind == 3u32.to_be_bytes());
            if !replaced_front {
                blocks.push((ty, body));
            }
        }
        position += 4 + length as u64;
    }
    Ok(blocks)
}
fn digest_range<F: EditableTagFile>(
    file: &mut CheckedIo<'_, F>,
    start: u64,
    end: u64,
) -> Result<String> {
    file.seek(SeekFrom::Start(start))?;
    let mut digest = Sha256::new();
    let mut buffer = [0u8; IO_BYTES];
    let mut position = start;
    while position < end {
        let count = usize::try_from((end - position).min(IO_BYTES as u64))?;
        file.read_exact(&mut buffer[..count])?;
        digest.update(&buffer[..count]);
        position += count as u64;
    }
    Ok(format!("{:x}", digest.finalize()))
}
fn verify_decode<F: EditableTagFile>(file: &mut CheckedIo<'_, F>, layout: &Layout) -> Result<()> {
    file.seek(SeekFrom::Start(layout.audio_start))?;
    let mut prefix = Vec::with_capacity(IO_BYTES + 42);
    if let Some(info) = &layout.stream_info {
        prefix.extend_from_slice(b"fLaC\x80\0\0\x22");
        prefix.extend_from_slice(info);
    }
    let count = usize::try_from((layout.length - layout.audio_start).min(IO_BYTES as u64))?;
    let offset = prefix.len();
    prefix.resize(offset + count, 0);
    file.read_exact(&mut prefix[offset..])?;
    let length = prefix.len() as u64;
    let mut decoder = Decoder::builder()
        .with_data(Cursor::new(prefix))
        .with_byte_len(length)
        .with_seekable(true)
        .build()
        .context("Written tagged audio has no valid decoder")?;
    ensure!(
        decoder.next().is_some_and(|sample| sample.is_finite()),
        "Written tagged audio has no finite decoded sample"
    );
    Ok(())
}
#[derive(Default)]
struct MetadataCounter {
    bytes: usize,
}
impl Write for MetadataCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("Tag metadata size overflow"))?;
        if self.bytes > MAX_TAG_METADATA_BYTES {
            return Err(io::Error::other("Encoded tag metadata exceeds 16 MiB"));
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct CheckedIo<'a, F> {
    file: &'a mut F,
    context: &'a ScanContext,
    read_remaining: Option<u64>,
}
impl<F: EditableTagFile> CheckedIo<'_, F> {
    fn check(&self) -> io::Result<()> {
        self.context.check().map_err(io::Error::other)?;
        self.file.check_write_authority()
    }
}
impl<F: EditableTagFile> Read for CheckedIo<'_, F> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.check()?;
        let limit = bytes.len().min(IO_BYTES).min(
            self.read_remaining
                .unwrap_or(u64::MAX)
                .min(usize::MAX as u64) as usize,
        );
        if limit == 0 && !bytes.is_empty() {
            return Err(io::Error::other("Tag metadata read exceeds 16 MiB"));
        }
        let count = self.file.read(&mut bytes[..limit])?;
        if let Err(error) = self.check() {
            bytes[..count].fill(0);
            return Err(error);
        }
        if let Some(remaining) = self.read_remaining.as_mut() {
            *remaining -= count as u64;
        }
        Ok(count)
    }
}
impl<F: EditableTagFile> Write for CheckedIo<'_, F> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.check()?;
        let limit = bytes.len().min(IO_BYTES);
        let position = self.file.stream_position()?;
        if position
            .checked_add(limit as u64)
            .is_none_or(|end| end > MAX_TAG_FILE_BYTES + MAX_TAG_METADATA_BYTES as u64)
        {
            return Err(io::Error::other("Editable tag body exceeds its size limit"));
        }
        let count = self.file.write(&bytes[..limit])?;
        self.check()?;
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.check()?;
        self.file.flush()?;
        self.check()
    }
}
impl<F: EditableTagFile> Seek for CheckedIo<'_, F> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.check()?;
        let position = self.file.seek(position)?;
        self.check()?;
        Ok(position)
    }
}
impl<F: EditableTagFile> Length for CheckedIo<'_, F> {
    fn len(&self) -> io::Result<u64> {
        self.check()?;
        let length = Length::len(self.file)?;
        self.check()?;
        Ok(length)
    }
}
impl<F: EditableTagFile> Truncate for CheckedIo<'_, F> {
    fn truncate(&mut self, length: u64) -> io::Result<()> {
        self.check()?;
        if length > MAX_TAG_FILE_BYTES + MAX_TAG_METADATA_BYTES as u64 {
            return Err(io::Error::other("Editable tag body exceeds its size limit"));
        }
        Truncate::truncate(self.file, length)?;
        self.check()
    }
}
