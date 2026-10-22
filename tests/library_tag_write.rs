#[path = "support/encoded_audio_fixture.rs"]
mod encoded;

use image::ImageEncoder;
use lofty::{
    TextEncoding,
    config::{ParseOptions, WriteOptions},
    file::{AudioFile, TaggedFileExt},
    flac::FlacFile,
    id3::v2::{Frame, FrameId, Id3v2Tag, TextInformationFrame, UnsynchronizedTextFrame},
    io::{Length, Truncate},
    ogg::{OggPictureStorage, tag::VorbisComments},
    picture::{Picture, PictureType},
    probe::Probe,
    tag::{ItemKey, TagExt, TagType},
};
use lx_gpui_player::library::{
    scan::ScanContext,
    tag_write::{
        EditableTagFile, LocalTagPatch, TagField, TagWriteFormat, VerifiedTagCover,
        write_local_tags,
    },
};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

struct OwnedCopy {
    file: File,
    allowed: Arc<AtomicBool>,
    max_io: Arc<AtomicUsize>,
    writes: usize,
    cancel_on_write: Option<Arc<AtomicBool>>,
    fail_after_write: Option<usize>,
}
impl OwnedCopy {
    fn create(path: &Path, bytes: &[u8]) -> Self {
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .unwrap();
        file.write_all(bytes).unwrap();
        file.rewind().unwrap();
        Self {
            file,
            allowed: Arc::new(AtomicBool::new(true)),
            max_io: Arc::new(AtomicUsize::new(0)),
            writes: 0,
            cancel_on_write: None,
            fail_after_write: None,
        }
    }
}
impl EditableTagFile for OwnedCopy {
    fn check_write_authority(&self) -> io::Result<()> {
        if !self.allowed.load(Ordering::Acquire) || !self.file.metadata()?.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Owned copy mutation lease is unavailable",
            ));
        }
        Ok(())
    }
    fn sync_data(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}
impl Read for OwnedCopy {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.max_io.fetch_max(bytes.len(), Ordering::AcqRel);
        self.file.read(bytes)
    }
}
impl Write for OwnedCopy {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.max_io.fetch_max(bytes.len(), Ordering::AcqRel);
        if self
            .fail_after_write
            .is_some_and(|limit| self.writes >= limit)
        {
            return Err(io::Error::other("Synthetic disk-full boundary"));
        }
        let count = self.file.write(bytes)?;
        self.writes += count;
        if let Some(cancel) = &self.cancel_on_write {
            cancel.store(true, Ordering::Release);
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}
impl Seek for OwnedCopy {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.file.seek(position)
    }
}
impl Length for OwnedCopy {
    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }
}
impl Truncate for OwnedCopy {
    fn truncate(&mut self, length: u64) -> io::Result<()> {
        self.file.set_len(length)
    }
}
fn context() -> ScanContext {
    ScanContext::new(Duration::from_secs(30))
}
fn png() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&[12, 44, 88], 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    bytes
}
fn tags(path: &Path) -> lofty::file::TaggedFile {
    Probe::new(File::open(path).unwrap())
        .guess_file_type()
        .unwrap()
        .read()
        .unwrap()
}
fn raw_flac_comments(path: &Path) -> VorbisComments {
    FlacFile::read_from(
        &mut File::open(path).unwrap(),
        ParseOptions::new().read_properties(false),
    )
    .unwrap()
    .vorbis_comments()
    .cloned()
    .unwrap_or_default()
}
fn patch() -> LocalTagPatch {
    LocalTagPatch {
        title: TagField::Set("合法合成标题".into()),
        artists: TagField::Set(vec!["作者甲".into(), "Artist B".into()]),
        album: TagField::Set("合成校验专辑".into()),
        lyrics: TagField::Set("[00:00.00]原创歌词\n[00:00.10]第二行".into()),
        cover: TagField::Set(VerifiedTagCover::from_bytes(png()).unwrap()),
    }
}

#[test]
fn actual_mp3_title_artists_album_zho_lyrics_and_cover_preserve_encoded_payload_and_other_frames() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("owned.mp3");
    let mut copy = OwnedCopy::create(&path, &encoded::silent_mpeg_layer3());
    let mut seed = Id3v2Tag::default();
    seed.insert(Frame::Text(TextInformationFrame::new(
        FrameId::Valid("TCON".into()),
        TextEncoding::UTF8,
        "Synthetic genre",
    )));
    seed.insert(Frame::UnsynchronizedText(UnsynchronizedTextFrame::new(
        TextEncoding::UTF8,
        *b"eng",
        "other language",
        "Retained English",
    )));
    seed.save_to(&mut copy.file, WriteOptions::default())
        .unwrap();
    let patch = patch();
    let verified = write_local_tags(&mut copy, &patch, &context()).unwrap();
    assert_eq!(verified.format, TagWriteFormat::Mp3);
    assert_eq!(
        verified.audio_sha256,
        format!("{:x}", Sha256::digest(encoded::silent_mpeg_layer3()))
    );
    assert_eq!(
        verified.body_sha256,
        format!("{:x}", Sha256::digest(fs::read(&path).unwrap()))
    );
    assert_eq!(verified.size_bytes, fs::metadata(&path).unwrap().len());
    let tagged = tags(&path);
    let tag = tagged.tag(TagType::Id3v2).unwrap();
    assert_eq!(tag.get_string(ItemKey::TrackTitle), Some("合法合成标题"));
    assert_eq!(tag.get_string(ItemKey::Genre), Some("Synthetic genre"));
    assert_eq!(
        tag.get_strings(ItemKey::TrackArtist).collect::<Vec<_>>(),
        vec!["作者甲", "Artist B"]
    );
    let id3: Id3v2Tag = tag.clone().into();
    assert!(
        id3.unsync_text()
            .any(|text| text.language == *b"eng" && text.content == "Retained English")
    );
    assert!(id3.unsync_text().any(|text| text.language == *b"zho"
        && text.description.is_empty()
        && text.content == "[00:00.00]原创歌词\n[00:00.10]第二行"));
    assert_eq!(
        tag.get_picture_type(PictureType::CoverFront)
            .unwrap()
            .data(),
        png()
    );
    assert!(copy.max_io.load(Ordering::Acquire) <= 64 * 1024);
    let remove = LocalTagPatch {
        title: TagField::Remove,
        artists: TagField::Remove,
        album: TagField::Remove,
        lyrics: TagField::Remove,
        cover: TagField::Remove,
    };
    let removed = write_local_tags(&mut copy, &remove, &context()).unwrap();
    assert_eq!(removed.audio_sha256, verified.audio_sha256);
    let tag = tags(&path).remove(TagType::Id3v2).unwrap();
    let id3: Id3v2Tag = tag.into();
    assert!(id3.unsync_text().any(|text| text.language == *b"eng"));
    assert!(
        !id3.unsync_text()
            .any(|text| text.language == *b"zho" && text.description.is_empty())
    );
}

#[test]
fn actual_flac_vorbis_lyrics_cover_and_unknown_comment_preserve_streaminfo_and_audio_frames() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("owned.flac");
    let original = encoded::mono_verbatim_flac();
    let mut copy = OwnedCopy::create(&path, &original);
    let mut seed = VorbisComments::default();
    seed.push("SYNTHETIC_KEEP".into(), "kept value".into());
    let mut back = Picture::from_reader(&mut png().as_slice()).unwrap();
    back.set_pic_type(PictureType::CoverBack);
    seed.insert_picture(back, None).unwrap();
    seed.save_to(&mut copy.file, WriteOptions::default())
        .unwrap();
    assert_eq!(
        raw_flac_comments(&path).get("SYNTHETIC_KEEP"),
        Some("kept value"),
        "The actual original file must contain the raw unknown comment before mutation"
    );
    let patch = patch();
    let verified = write_local_tags(&mut copy, &patch, &context()).unwrap();
    assert_eq!(verified.format, TagWriteFormat::Flac);
    let tagged = tags(&path);
    let tag = tagged.tag(TagType::VorbisComments).unwrap();
    assert_eq!(tag.get_string(ItemKey::TrackTitle), Some("合法合成标题"));
    assert_eq!(
        tag.get_string(ItemKey::Lyrics),
        Some("[00:00.00]原创歌词\n[00:00.10]第二行")
    );
    let comments = raw_flac_comments(&path);
    assert_eq!(comments.get("SYNTHETIC_KEEP"), Some("kept value"));
    assert_eq!(
        tag.get_picture_type(PictureType::CoverBack).unwrap().data(),
        png()
    );
    assert_eq!(
        tag.get_picture_type(PictureType::CoverFront)
            .unwrap()
            .data(),
        png()
    );
    assert_eq!(
        verified.body_sha256,
        format!("{:x}", Sha256::digest(fs::read(&path).unwrap()))
    );
    assert!(copy.max_io.load(Ordering::Acquire) <= 64 * 1024);
    let removed = write_local_tags(
        &mut copy,
        &LocalTagPatch {
            lyrics: TagField::Remove,
            cover: TagField::Remove,
            ..Default::default()
        },
        &context(),
    )
    .unwrap();
    assert_eq!(removed.audio_sha256, verified.audio_sha256);
    let tagged = tags(&path);
    let tag = tagged.tag(TagType::VorbisComments).unwrap();
    assert!(tag.get_string(ItemKey::Lyrics).is_none());
    assert!(tag.get_picture_type(PictureType::CoverFront).is_none());
    assert!(tag.get_picture_type(PictureType::CoverBack).is_some());
}

#[test]
fn denied_write_unsupported_body_expired_context_and_decode_bomb_never_return_a_verified_product() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let path = temp.path().join("denied.mp3");
    let original = encoded::silent_mpeg_layer3();
    let mut copy = OwnedCopy::create(&path, &original);
    copy.allowed.store(false, Ordering::Release);
    assert!(write_local_tags(&mut copy, &patch(), &context()).is_err());
    assert_eq!(fs::read(&path).unwrap(), original);
    assert_eq!(copy.writes, 0);
    copy.allowed.store(true, Ordering::Release);
    assert!(write_local_tags(&mut copy, &patch(), &ScanContext::new(Duration::ZERO)).is_err());
    assert_eq!(copy.writes, 0);
    let wave = encoded::mono_pcm32_wav(&[0, 1, -1, 7]);
    let wave_path = temp.path().join("unsupported.wav");
    let mut unsupported = OwnedCopy::create(&wave_path, &wave);
    let error = write_local_tags(&mut unsupported, &patch(), &context()).unwrap_err();
    assert!(format!("{error:#}").contains("UnsupportedCapability"));
    assert_eq!(unsupported.writes, 0);
    assert_eq!(fs::read(&wave_path).unwrap(), wave);
    assert!(VerifiedTagCover::from_bytes(vec![0; 8 * 1024 * 1024 + 1]).is_err());
    let mut oversized = Vec::new();
    image::codecs::png::PngEncoder::new(&mut oversized)
        .write_image(&vec![0; 4097 * 3], 4097, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    assert!(VerifiedTagCover::from_bytes(oversized).is_err());
}

#[test]
fn cancellation_and_actual_writer_failure_discard_the_copy_without_mutating_the_original() {
    let temp = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
    let original = encoded::silent_mpeg_layer3();
    let good = temp.path().join("original.mp3");
    fs::write(&good, &original).unwrap();
    let cancel_context = context();
    let mut cancelled = OwnedCopy::create(&temp.path().join("cancelled.mp3"), &original);
    cancelled.cancel_on_write = Some(cancel_context.cancel.clone());
    assert!(write_local_tags(&mut cancelled, &patch(), &cancel_context).is_err());
    assert!(cancelled.writes > 0);
    assert_eq!(fs::read(&good).unwrap(), original);
    let mut failed = OwnedCopy::create(&temp.path().join("failed.mp3"), &original);
    failed.fail_after_write = Some(1);
    assert!(write_local_tags(&mut failed, &patch(), &context()).is_err());
    assert!(failed.writes > 0);
    assert_eq!(fs::read(&good).unwrap(), original);
    assert!(failed.max_io.load(Ordering::Acquire) <= 64 * 1024);
}
