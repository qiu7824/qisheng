//! Actual encoded artwork must pass full decoding before it gains a private proof.
use image::{
    Delay, ExtendedColorType, Frame, ImageEncoder, ImageFormat, Rgba, RgbaImage,
    codecs::{gif::GifEncoder, jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder},
};
use lx_gpui_player::{
    domain::artwork::MAX_ARTWORK_BYTES,
    library::artwork_validator::{
        MAX_ANIMATION_PIXELS, MAX_IMAGE_DIMENSION, MAX_IMAGE_FRAMES, validate_artwork,
    },
};
use sha2::{Digest, Sha256};
use std::{cell::Cell, fmt, io::Write, sync::Arc};

fn pixels(width: u32, height: u32, phase: u8) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| {
        Rgba([
            (x * 17) as u8,
            (y * 13) as u8,
            phase.wrapping_add((x + y) as u8),
            255,
        ])
    })
}
fn encoded(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let image = pixels(width, height, 47);
    let mut bytes = Vec::new();
    match format {
        ImageFormat::Png => PngEncoder::new(&mut bytes)
            .write_image(image.as_raw(), width, height, ExtendedColorType::Rgba8)
            .unwrap(),
        ImageFormat::Jpeg => {
            let rgb: Vec<_> = image
                .pixels()
                .flat_map(|pixel| pixel.0[..3].to_vec())
                .collect();
            JpegEncoder::new_with_quality(&mut bytes, 90)
                .encode(&rgb, width, height, ExtendedColorType::Rgb8)
                .unwrap();
        }
        ImageFormat::Gif => {
            let mut encoder = GifEncoder::new(&mut bytes);
            encoder.encode_frame(Frame::new(image)).unwrap();
        }
        ImageFormat::WebP => WebPEncoder::new_lossless(&mut bytes)
            .encode(image.as_raw(), width, height, ExtendedColorType::Rgba8)
            .unwrap(),
        _ => unreachable!(),
    }
    bytes
}
fn formats() -> [(ImageFormat, &'static str); 4] {
    [
        (ImageFormat::Png, "image/png"),
        (ImageFormat::Jpeg, "image/jpeg"),
        (ImageFormat::Gif, "image/gif"),
        (ImageFormat::WebP, "image/webp"),
    ]
}
fn gif(frames: u32) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = GifEncoder::new(&mut bytes);
        for phase in 0..frames {
            encoder
                .encode_frame(Frame::from_parts(
                    pixels(16, 12, phase as u8),
                    0,
                    0,
                    Delay::from_numer_denom_ms(20, 1),
                ))
                .unwrap();
        }
    }
    bytes
}

fn crc(bytes: &[u8]) -> u32 {
    let mut result = u32::MAX;
    for byte in bytes {
        result ^= u32::from(*byte);
        for _ in 0..8 {
            result = if result & 1 == 0 {
                result >> 1
            } else {
                (result >> 1) ^ 0xedb8_8320
            };
        }
    }
    !result
}
fn png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    output.extend((payload.len() as u32).to_be_bytes());
    output.extend(kind);
    output.extend(payload);
    let start = output.len() - payload.len() - 4;
    output.extend(crc(&output[start..]).to_be_bytes());
}
fn compressed_rows(width: u32, height: u32, phase: u8) -> Vec<u8> {
    let mut encoder = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut row = Vec::with_capacity(width as usize * 4 + 1);
    row.push(0);
    for _ in 0..width {
        row.extend([phase, 63, 127, 255]);
    }
    for _ in 0..height {
        encoder.write_all(&row).unwrap();
    }
    encoder.finish().unwrap()
}
fn frame_control(sequence: u32, width: u32, height: u32) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend(sequence.to_be_bytes());
    data.extend(width.to_be_bytes());
    data.extend(height.to_be_bytes());
    data.extend(0_u32.to_be_bytes());
    data.extend(0_u32.to_be_bytes());
    data.extend(1_u16.to_be_bytes());
    data.extend(50_u16.to_be_bytes());
    data.extend([0, 0]);
    data
}
fn apng(
    width: u32,
    height: u32,
    frames: u32,
    separate_default: bool,
    bad_default: bool,
    bad_last: bool,
) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::new();
    header.extend(width.to_be_bytes());
    header.extend(height.to_be_bytes());
    header.extend([8, 6, 0, 0, 0]);
    png_chunk(&mut bytes, b"IHDR", &header);
    let mut animation = Vec::new();
    animation.extend(frames.to_be_bytes());
    animation.extend(0_u32.to_be_bytes());
    png_chunk(&mut bytes, b"acTL", &animation);
    let mut sequence = 0;
    if separate_default {
        let data = if bad_default {
            vec![0x78, 0x9c, 0xff]
        } else {
            compressed_rows(width, height, 191)
        };
        png_chunk(&mut bytes, b"IDAT", &data);
    }
    for index in 0..frames {
        png_chunk(&mut bytes, b"fcTL", &frame_control(sequence, width, height));
        sequence += 1;
        let mut data = compressed_rows(width, height, index as u8);
        if bad_last && index == frames - 1 {
            data.truncate(data.len() / 2);
        }
        if index == 0 && !separate_default {
            png_chunk(&mut bytes, b"IDAT", &data);
        } else {
            let mut framed = sequence.to_be_bytes().to_vec();
            sequence += 1;
            framed.extend(data);
            png_chunk(&mut bytes, b"fdAT", &framed);
        }
    }
    png_chunk(&mut bytes, b"IEND", &[]);
    bytes
}
fn riff_chunk(output: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
    output.extend(kind);
    output.extend((payload.len() as u32).to_le_bytes());
    output.extend(payload);
    if !payload.len().is_multiple_of(2) {
        output.push(0);
    }
}
fn little24(value: u32) -> [u8; 3] {
    let bytes = value.to_le_bytes();
    [bytes[0], bytes[1], bytes[2]]
}
fn animated_webp(frames: u32, bad_last: bool) -> Vec<u8> {
    let width = 16;
    let height = 12;
    let mut bytes = b"RIFF\0\0\0\0WEBP".to_vec();
    let mut extended = vec![0x12, 0, 0, 0];
    extended.extend(little24(width - 1));
    extended.extend(little24(height - 1));
    riff_chunk(&mut bytes, b"VP8X", &extended);
    riff_chunk(&mut bytes, b"ANIM", &[0, 0, 0, 0, 0, 0]);
    for index in 0..frames {
        let image = pixels(width, height, index as u8);
        let mut still = Vec::new();
        WebPEncoder::new_lossless(&mut still)
            .encode(image.as_raw(), width, height, ExtendedColorType::Rgba8)
            .unwrap();
        let mut frame = Vec::new();
        frame.extend(little24(0));
        frame.extend(little24(0));
        frame.extend(little24(width - 1));
        frame.extend(little24(height - 1));
        frame.extend(little24(20));
        frame.push(2);
        let mut payload = still[12..].to_vec();
        if bad_last && index == frames - 1 {
            payload.truncate(payload.len() / 2);
        }
        frame.extend(payload);
        riff_chunk(&mut bytes, b"ANMF", &frame);
    }
    let length = (bytes.len() as u32 - 8).to_le_bytes();
    bytes[4..8].copy_from_slice(&length);
    bytes
}

#[test]
fn real_png_jpeg_gif_webp_pixels_receive_exact_immutable_encoded_proofs() {
    for (format, mime) in formats() {
        let bytes = encoded(format, 16, 12);
        let expected = bytes.clone();
        let verified = validate_artwork(bytes, Some(mime), || Ok(())).unwrap();
        let metadata = verified.metadata();
        assert_eq!(metadata.mime_type, mime);
        assert_eq!(
            (metadata.width, metadata.height, metadata.frames),
            (16, 12, 1)
        );
        assert_eq!(metadata.encoded_bytes, expected.len() as u64);
        assert_eq!(metadata.sha256, format!("{:x}", Sha256::digest(&expected)));
        assert_eq!(verified.encoded(), expected);
        let mut external = verified.encoded_arc();
        let second = verified.clone().encoded_arc();
        assert!(Arc::ptr_eq(&external, &second));
        Arc::make_mut(&mut external)[0] ^= 0xff;
        assert_eq!(verified.encoded(), expected);
    }
}

#[test]
fn declared_mime_accepts_only_the_decoded_format_or_generic_binary() {
    for (format, mime) in formats() {
        let bytes = encoded(format, 16, 12);
        for declared in [None, Some(mime), Some("application/octet-stream")] {
            validate_artwork(bytes.clone(), declared, || Ok(())).unwrap();
        }
        let normalized = format!(" {} ; charset=binary", mime.to_ascii_uppercase());
        validate_artwork(bytes.clone(), Some(&normalized), || Ok(())).unwrap();
        for incorrect in [
            "text/html",
            "image/svg+xml",
            if mime == "image/jpeg" {
                "image/png"
            } else {
                "image/jpeg"
            },
        ] {
            assert!(validate_artwork(bytes.clone(), Some(incorrect), || Ok(())).is_err());
        }
    }
}

#[test]
fn recognized_magic_and_plausible_end_markers_do_not_replace_complete_decoding() {
    for (format, mime) in formats() {
        let original = encoded(format, 16, 12);
        let mut truncated = original[..original.len() / 2].to_vec();
        assert!(validate_artwork(truncated.clone(), Some(mime), || Ok(())).is_err());
        match format {
            ImageFormat::Jpeg => truncated.extend([0xff, 0xd9]),
            ImageFormat::Gif => truncated.push(0x3b),
            ImageFormat::WebP => {
                let length = (truncated.len() as u32 - 8).to_le_bytes();
                truncated[4..8].copy_from_slice(&length);
            }
            _ => {}
        }
        assert!(validate_artwork(truncated, Some(mime), || Ok(())).is_err());
    }
}

#[test]
fn png_checksums_and_complete_end_marker_are_required() {
    let original = encoded(ImageFormat::Png, 16, 12);
    let mut corrupt = original.clone();
    corrupt[29] ^= 1;
    assert!(validate_artwork(corrupt, None, || Ok(())).is_err());
    let mut missing_end = original.clone();
    missing_end.truncate(missing_end.len() - 12);
    assert!(validate_artwork(missing_end, None, || Ok(())).is_err());
    let mut appended = original;
    appended.extend(b"unowned trailing data");
    assert!(validate_artwork(appended, None, || Ok(())).is_err());
}

#[test]
fn actual_oversized_images_and_compressed_pixel_bombs_are_rejected() {
    let oversized = encoded(ImageFormat::Png, MAX_IMAGE_DIMENSION + 1, 1);
    assert!(validate_artwork(oversized, Some("image/png"), || Ok(())).is_err());
    let size = MAX_IMAGE_DIMENSION + 1;
    let pixels = vec![0; size as usize * size as usize];
    let mut compressed = Vec::new();
    PngEncoder::new(&mut compressed)
        .write_image(&pixels, size, size, ExtendedColorType::L8)
        .unwrap();
    assert!(compressed.len() < MAX_ARTWORK_BYTES);
    assert!(validate_artwork(compressed, Some("image/png"), || Ok(())).is_err());
}

#[test]
fn encoded_byte_limit_precedes_parsing_large_valid_ancillary_data() {
    let original = encoded(ImageFormat::Png, 16, 12);
    let end = original.len() - 12;
    let mut bytes = original[..end].to_vec();
    let mut text = b"Comment\0".to_vec();
    text.resize(MAX_ARTWORK_BYTES + 1, b'A');
    png_chunk(&mut bytes, b"tEXt", &text);
    bytes.extend(&original[end..]);
    let checks = Cell::new(0);
    assert!(
        validate_artwork(bytes, None, || {
            checks.set(checks.get() + 1);
            Ok(())
        })
        .is_err()
    );
    assert_eq!(checks.get(), 1);
}

#[test]
fn complete_gif_apng_webp_animation_sequences_obey_the_frame_limit() {
    for mime in ["image/gif", "image/png", "image/webp"] {
        let build = |frames| match mime {
            "image/gif" => gif(frames),
            "image/png" => apng(16, 12, frames, false, false, false),
            _ => animated_webp(frames, false),
        };
        let valid = validate_artwork(build(MAX_IMAGE_FRAMES), Some(mime), || Ok(())).unwrap();
        assert_eq!(valid.metadata().frames, MAX_IMAGE_FRAMES);
        assert!(validate_artwork(build(MAX_IMAGE_FRAMES + 1), Some(mime), || Ok(())).is_err());
    }
}

#[test]
fn apng_default_thumbnail_and_later_animation_frames_are_independently_decoded() {
    let valid = validate_artwork(
        apng(16, 12, 2, true, false, false),
        Some("image/png"),
        || Ok(()),
    )
    .unwrap();
    assert_eq!(valid.metadata().frames, 2);
    assert!(
        validate_artwork(
            apng(16, 12, 2, true, true, false),
            Some("image/png"),
            || Ok(())
        )
        .is_err()
    );
    assert!(
        validate_artwork(
            apng(16, 12, 2, false, false, true),
            Some("image/png"),
            || Ok(())
        )
        .is_err()
    );
    assert!(validate_artwork(animated_webp(2, true), Some("image/webp"), || Ok(())).is_err());
}

#[test]
fn cumulative_animation_pixels_are_bounded_even_when_each_frame_is_small_enough() {
    let pixels = 1024_u64 * 1024;
    let accepted = (MAX_ANIMATION_PIXELS / pixels) as u32;
    let bytes = apng(1024, 1024, accepted + 1, false, false, false);
    assert!(bytes.len() < MAX_ARTWORK_BYTES);
    assert!(validate_artwork(bytes, Some("image/png"), || Ok(())).is_err());
}

#[derive(Debug)]
struct ValidationCancelled;
impl fmt::Display for ValidationCancelled {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Artwork operation cancelled")
    }
}
impl std::error::Error for ValidationCancelled {}

#[test]
fn caller_cancellation_is_preserved_before_decode_between_frames_and_before_publication() {
    let immediate = validate_artwork(encoded(ImageFormat::Png, 16, 12), None, || {
        Err(ValidationCancelled.into())
    })
    .unwrap_err();
    assert!(immediate.is::<ValidationCancelled>());
    let checks = Cell::new(0);
    let between_frames = validate_artwork(gif(8), None, || {
        let count = checks.get() + 1;
        checks.set(count);
        if count >= 4 {
            Err(ValidationCancelled.into())
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(between_frames.is::<ValidationCancelled>());
    assert_eq!(checks.get(), 4);
    let checks = Cell::new(0);
    let before_publish = validate_artwork(encoded(ImageFormat::Jpeg, 16, 12), None, || {
        checks.set(checks.get() + 1);
        if checks.get() >= 2 {
            Err(ValidationCancelled.into())
        } else {
            Ok(())
        }
    })
    .unwrap_err();
    assert!(before_publish.is::<ValidationCancelled>());
}

#[test]
fn empty_html_svg_and_unsupported_raster_formats_never_receive_a_verified_proof() {
    for bytes in [
        Vec::new(),
        b"<html>original text</html>".to_vec(),
        b"<svg xmlns='http://www.w3.org/2000/svg' width='1' height='1'/>".to_vec(),
    ] {
        assert!(validate_artwork(bytes, None, || Ok(())).is_err());
    }
    let mut bitmap = b"BM".to_vec();
    bitmap.extend(58_u32.to_le_bytes());
    bitmap.extend([0; 4]);
    bitmap.extend(54_u32.to_le_bytes());
    bitmap.extend(40_u32.to_le_bytes());
    bitmap.extend(1_i32.to_le_bytes());
    bitmap.extend(1_i32.to_le_bytes());
    bitmap.extend(1_u16.to_le_bytes());
    bitmap.extend(24_u16.to_le_bytes());
    bitmap.extend([0; 24]);
    bitmap.extend([12, 34, 56, 0]);
    assert!(validate_artwork(bitmap, None, || Ok(())).is_err());
}
