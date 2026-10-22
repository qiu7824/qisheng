use base64::Engine;
use encoding_rs::GBK;
use lofty::config::WriteOptions;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{ItemKey, ItemValue, Tag, TagExt, TagItem, TagType};
use lx_gpui_player::catalog::Catalog;
use lx_gpui_player::library::metadata::read_local_metadata;
use lx_gpui_player::lyrics::local::{
    LocalLyricProvenance, LyricEncoding, MAX_SIDECAR_BYTES, decode_lyric_bundle,
    decode_lyric_bytes, read_local_lyrics,
};
use std::io::Write;
use std::path::Path;

fn original_wav(path: &Path) {
    let data_bytes = 44_100_u32;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&(36 + data_bytes).to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&22_050_u32.to_le_bytes());
    bytes.extend_from_slice(&44_100_u32.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&data_bytes.to_le_bytes());
    bytes.resize(44 + data_bytes as usize, 0);
    std::fs::write(path, bytes).unwrap();
}

fn original_png() -> Vec<u8> {
    fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        output.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = output.len();
        output.extend_from_slice(kind);
        output.extend_from_slice(data);
        let mut crc = !0_u32;
        for byte in &output[start..] {
            crc ^= *byte as u32;
            for _ in 0..8 {
                crc = (crc >> 1) ^ (0xedb88320 & (0_u32.wrapping_sub(crc & 1)));
            }
        }
        output.extend_from_slice(&(!crc).to_be_bytes());
    }
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&1_u32.to_be_bytes());
    ihdr.extend_from_slice(&1_u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    let mut encoded = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoded.write_all(&[0, 255, 32, 64, 255]).unwrap();
    chunk(&mut png, b"IDAT", &encoded.finish().unwrap());
    chunk(&mut png, b"IEND", &[]);
    png
}

fn write_original_tags(path: &Path, lyric: &str) -> Vec<u8> {
    let mut tag = Tag::new(TagType::Id3v2);
    tag.insert_text(ItemKey::TrackTitle, "真实标题".into());
    tag.push(TagItem::new(
        ItemKey::TrackArtist,
        ItemValue::Text("作者甲".into()),
    ));
    tag.push(TagItem::new(
        ItemKey::TrackArtist,
        ItemValue::Text("作者乙".into()),
    ));
    tag.insert_text(ItemKey::AlbumTitle, "原创测试专辑".into());
    tag.insert_text(ItemKey::UnsyncLyrics, lyric.into());
    let png = original_png();
    tag.push_picture(
        Picture::unchecked(png.clone())
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Png)
            .build(),
    );
    tag.save_to_path(path, WriteOptions::default()).unwrap();
    png
}

#[test]
fn selected_wav_id3_tags_artwork_and_embedded_lyrics_are_read_and_used_by_catalog() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("文件名.wav");
    original_wav(&path);
    let lyric = "[00:00.000]原创歌词\n[00:00.500]第二句";
    let png = write_original_tags(&path, lyric);
    let metadata = read_local_metadata(&path).unwrap();
    assert_eq!(metadata.title.as_deref(), Some("真实标题"));
    assert_eq!(metadata.album.as_deref(), Some("原创测试专辑"));
    assert_eq!(metadata.artists, ["作者甲", "作者乙"]);
    assert_eq!(metadata.duration_ms, 1000);
    assert_eq!(metadata.pictures.len(), 1);
    assert_eq!(metadata.pictures[0].mime_type, "image/png");
    assert_eq!(metadata.pictures[0].data, png);
    assert_eq!(metadata.embedded_lyrics.unwrap().lyric.text(), Some(lyric));
    assert!(metadata.warnings.is_empty());

    let lyrics = read_local_lyrics(&path).unwrap().unwrap();
    assert_eq!(lyrics.provenance, LocalLyricProvenance::Embedded);
    assert_eq!(lyrics.bundle.lyric.text(), Some(lyric));
    assert!(lyrics.sidecar_path.is_none());
    let catalog = Catalog::new().unwrap();
    let prepared = catalog.prepare_local(path).unwrap();
    assert!(prepared.warnings().is_empty());
    let track = catalog.commit_local(prepared);
    assert_eq!(track.title, "真实标题");
    assert_eq!(track.artist, "作者甲 / 作者乙");
    assert_eq!(track.artists, ["作者甲", "作者乙"]);
    assert_eq!(track.album, "原创测试专辑");
}

#[test]
fn same_name_lrc_overrides_embedded_lyrics_and_supports_declared_encodings() {
    let directory = tempfile::tempdir().unwrap();
    let audio = directory.path().join("selected.wav");
    original_wav(&audio);
    write_original_tags(&audio, "[00:00.000]内嵌原文");
    let text = "[00:01.250]旁车歌词";
    let utf16 = |little_endian: bool, bom: bool| {
        let mut bytes = if bom {
            if little_endian {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            }
        } else {
            Vec::new()
        };
        for word in text.encode_utf16() {
            bytes.extend_from_slice(&if little_endian {
                word.to_le_bytes()
            } else {
                word.to_be_bytes()
            });
        }
        bytes
    };
    let (gbk, _, had_errors) = GBK.encode(text);
    assert!(!had_errors);
    let mut utf8_bom = vec![0xef, 0xbb, 0xbf];
    utf8_bom.extend_from_slice(text.as_bytes());
    for (bytes, encoding) in [
        (text.as_bytes().to_vec(), LyricEncoding::Utf8),
        (utf8_bom, LyricEncoding::Utf8),
        (utf16(true, true), LyricEncoding::Utf16Le),
        (utf16(false, true), LyricEncoding::Utf16Be),
        (utf16(true, false), LyricEncoding::Utf16Le),
        (utf16(false, false), LyricEncoding::Utf16Be),
        (gbk.into_owned(), LyricEncoding::Gbk),
    ] {
        std::fs::write(audio.with_extension("lrc"), bytes).unwrap();
        let lyrics = read_local_lyrics(&audio).unwrap().unwrap();
        assert_eq!(lyrics.provenance, LocalLyricProvenance::Sidecar);
        assert_eq!(lyrics.encoding, Some(encoding));
        assert_eq!(lyrics.bundle.lyric.text(), Some(text));
    }
}

#[test]
fn absent_lyrics_invalid_encodings_and_oversized_sidecars_are_distinct() {
    let directory = tempfile::tempdir().unwrap();
    let audio = directory.path().join("selected.wav");
    original_wav(&audio);
    assert!(read_local_lyrics(&audio).unwrap().is_none());
    assert!(read_local_lyrics(directory.path().join("missing.wav")).is_err());
    for bytes in [
        vec![0xef, 0xbb, 0xbf, 0xff],
        vec![0xff, 0xfe, 0, 0xd8],
        vec![0xff],
    ] {
        assert!(decode_lyric_bytes(&bytes).is_err());
    }
    let sidecar = std::fs::File::create(audio.with_extension("lrc")).unwrap();
    sidecar.set_len(MAX_SIDECAR_BYTES as u64 + 1).unwrap();
    drop(sidecar);
    assert!(
        read_local_lyrics(&audio)
            .unwrap_err()
            .to_string()
            .contains("2 MiB")
    );
    std::fs::remove_file(audio.with_extension("lrc")).unwrap();
    std::fs::write(audio.with_extension("krc"), b"invalid KRC container").unwrap();
    assert!(
        read_local_lyrics(&audio)
            .unwrap_err()
            .to_string()
            .contains("KRC")
    );
}

#[test]
fn lx_four_channel_container_preserves_tracks_and_rejects_bad_base64() {
    let fields = [
        ("lrc", "[00:01.000]主歌词"),
        ("tlrc", "[00:01.000]Translation"),
        ("rlrc", "[00:01.000]Romanization"),
        ("awlrc", "[00:01.000]<0,500>主<500,500>歌词"),
    ];
    let encoded = fields
        .iter()
        .map(|(kind, text)| {
            format!(
                "{kind}:{}",
                base64::engine::general_purpose::STANDARD.encode(text.as_bytes())
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let bundle = decode_lyric_bundle(&format!("[awlrc:{encoded}]\n[00:09.000]outside")).unwrap();
    assert_eq!(bundle.lyric.text(), Some(fields[0].1));
    assert_eq!(bundle.tlyric.text(), Some(fields[1].1));
    assert_eq!(bundle.rlyric.text(), Some(fields[2].1));
    assert_eq!(bundle.lxlyric.text(), Some(fields[3].1));
    assert!(decode_lyric_bundle("[awlrc:lrc:!!!]").is_err());
    assert!(decode_lyric_bundle("[awlrc:lrc:bad").is_err());
}

#[test]
fn compressed_id3_frames_are_rejected_before_the_library_decompresses_them() {
    fn syncsafe(value: usize) -> [u8; 4] {
        [
            ((value >> 21) & 0x7f) as u8,
            ((value >> 14) & 0x7f) as u8,
            ((value >> 7) & 0x7f) as u8,
            (value & 0x7f) as u8,
        ]
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("compressed-tags.wav");
    original_wav(&path);
    let text = vec![b'x'; 65_536];
    let mut zlib = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    zlib.write_all(&[3]).unwrap();
    zlib.write_all(&text).unwrap();
    let mut frame_content = syncsafe(text.len() + 1).to_vec();
    frame_content.extend_from_slice(&zlib.finish().unwrap());
    let mut frame = b"TIT2".to_vec();
    frame.extend_from_slice(&syncsafe(frame_content.len()));
    frame.extend_from_slice(&[0, 0x09]);
    frame.extend_from_slice(&frame_content);
    let mut tag = b"ID3\x04\x00\x00".to_vec();
    tag.extend_from_slice(&syncsafe(frame.len()));
    tag.extend_from_slice(&frame);
    let mut wav = std::fs::read(&path).unwrap();
    wav.extend_from_slice(b"id3 ");
    wav.extend_from_slice(&(tag.len() as u32).to_le_bytes());
    wav.extend_from_slice(&tag);
    if !tag.len().is_multiple_of(2) {
        wav.push(0);
    }
    let size = wav.len() as u32 - 8;
    wav[4..8].copy_from_slice(&size.to_le_bytes());
    std::fs::write(&path, wav).unwrap();
    let error = read_local_metadata(path).unwrap_err().to_string();
    assert!(
        error.contains("UnsupportedCapability") && error.contains("压缩ID3"),
        "unexpected error: {error}"
    );
}

#[test]
fn corrupt_sidecar_falls_back_to_embedded_lyrics_with_a_warning() {
    let directory = tempfile::tempdir().unwrap();
    let audio = directory.path().join("selected.wav");
    original_wav(&audio);
    let embedded = "[00:00.000]内嵌歌词";
    write_original_tags(&audio, embedded);
    std::fs::write(audio.with_extension("lrc"), [0xef, 0xbb, 0xbf, 0xff]).unwrap();
    let lyrics = read_local_lyrics(audio).unwrap().unwrap();
    assert_eq!(lyrics.provenance, LocalLyricProvenance::Embedded);
    assert_eq!(lyrics.bundle.lyric.text(), Some(embedded));
    assert!(
        lyrics
            .warnings
            .iter()
            .any(|warning| warning.contains("UTF-8"))
    );
}
