//! Full, bounded decoding of encoded artwork. No image buffers are retained.
use crate::domain::artwork::{ArtworkBlobMeta, MAX_ARTWORK_BYTES};
use anyhow::{Context, Result, ensure};
use image::{AnimationDecoder, ImageDecoder, ImageFormat, Limits};
use sha2::{Digest, Sha256};
use std::{io::Cursor, sync::Arc};

pub const MAX_IMAGE_DIMENSION: u32 = 4096;
pub const MAX_IMAGE_PIXELS: u64 = 16 * 1024 * 1024;
pub const MAX_IMAGE_FRAMES: u32 = 64;
pub const MAX_ANIMATION_PIXELS: u64 = 32 * 1024 * 1024;
pub const MAX_DECODER_ALLOC_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone)]
pub struct VerifiedArtwork {
    metadata: ArtworkBlobMeta,
    encoded: Arc<[u8]>,
}
impl std::fmt::Debug for VerifiedArtwork {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedArtwork")
            .field("metadata", &self.metadata)
            .field("encoded_bytes", &self.encoded.len())
            .finish()
    }
}
impl VerifiedArtwork {
    pub fn metadata(&self) -> &ArtworkBlobMeta {
        &self.metadata
    }
    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }
    pub fn encoded_arc(&self) -> Arc<[u8]> {
        self.encoded.clone()
    }
}
fn limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODER_ALLOC_BYTES);
    limits
}
fn dimensions((width, height): (u32, u32)) -> Result<(u32, u32)> {
    ensure!(
        width > 0
            && height > 0
            && width <= MAX_IMAGE_DIMENSION
            && height <= MAX_IMAGE_DIMENSION
            && width as u64 * height as u64 <= MAX_IMAGE_PIXELS,
        "Artwork dimensions exceed their limit"
    );
    Ok((width, height))
}
fn still(decoder: impl ImageDecoder) -> Result<()> {
    let length = decoder.total_bytes();
    ensure!(
        length > 0 && length <= MAX_DECODER_ALLOC_BYTES,
        "Artwork decoded buffer exceeds its limit"
    );
    let mut pixels = vec![0; usize::try_from(length)?];
    decoder
        .read_image(&mut pixels)
        .context("Artwork could not be fully decoded")?;
    Ok(())
}
fn animation<'a>(frames: image::Frames<'a>, check: &impl Fn() -> Result<()>) -> Result<u32> {
    let mut count = 0u32;
    let mut pixels = 0u64;
    for frame in frames {
        check()?;
        let frame = frame.context("Artwork animation could not be fully decoded")?;
        count += 1;
        ensure!(
            count <= MAX_IMAGE_FRAMES,
            "Artwork animation exceeds 64 frames"
        );
        let buffer = frame.buffer();
        dimensions(buffer.dimensions())?;
        pixels = pixels
            .checked_add(buffer.width() as u64 * buffer.height() as u64)
            .context("Artwork animation size overflow")?;
        ensure!(
            pixels <= MAX_ANIMATION_PIXELS,
            "Artwork animation exceeds its decoded pixel budget"
        );
    }
    ensure!(count > 0, "Artwork animation has no decodable frames");
    Ok(count)
}
const fn crc_table() -> [u32; 256] {
    let mut table = [0; 256];
    let mut index = 0;
    while index < 256 {
        let mut crc = index as u32;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 != 0 {
                0xedb88320 ^ (crc >> 1)
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
    }
    table
}
const CRC_TABLE: [u32; 256] = crc_table();
fn png_envelope(bytes: &[u8], check: &impl Fn() -> Result<()>) -> Result<()> {
    let mut position = 8usize;
    let mut chunks = 0usize;
    let mut ended = false;
    while position < bytes.len() {
        check()?;
        chunks += 1;
        ensure!(chunks <= 4096, "Artwork PNG chunk count exceeds its limit");
        let header = bytes
            .get(position..position + 8)
            .context("Artwork PNG chunk is truncated")?;
        let length = u32::from_be_bytes(header[..4].try_into()?) as usize;
        let end = position
            .checked_add(12)
            .and_then(|value| value.checked_add(length))
            .context("Artwork PNG chunk size overflow")?;
        ensure!(end <= bytes.len(), "Artwork PNG chunk is truncated");
        let mut crc = 0xffff_ffffu32;
        for block in bytes[position + 4..end - 4].chunks(64 * 1024) {
            check()?;
            for byte in block {
                crc = CRC_TABLE[((crc ^ u32::from(*byte)) & 0xff) as usize] ^ (crc >> 8);
            }
        }
        let stored = u32::from_be_bytes(bytes[end - 4..end].try_into()?);
        ensure!(!crc == stored, "Artwork PNG checksum is invalid");
        if &header[4..8] == b"IEND" {
            ensure!(
                length == 0 && end == bytes.len(),
                "Artwork PNG end marker is invalid"
            );
            ended = true;
            break;
        }
        position = end;
    }
    ensure!(ended, "Artwork PNG is missing its end marker");
    Ok(())
}

pub fn validate_artwork(
    bytes: Vec<u8>,
    declared_mime: Option<&str>,
    check: impl Fn() -> Result<()>,
) -> Result<VerifiedArtwork> {
    check()?;
    ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_ARTWORK_BYTES,
        "Artwork encoded bytes exceed 8 MiB"
    );
    let format = image::guess_format(&bytes).context("Artwork is not a recognized image")?;
    let mime = match format {
        ImageFormat::Jpeg => "image/jpeg",
        ImageFormat::Png => "image/png",
        ImageFormat::Gif => "image/gif",
        ImageFormat::WebP => "image/webp",
        _ => anyhow::bail!("Artwork format is unsupported"),
    };
    if let Some(declared) = declared_mime {
        let declared = declared
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        ensure!(
            declared == mime || declared == "application/octet-stream",
            "Artwork MIME type does not match its encoded format"
        );
    }
    let (width, height, frames) = match format {
        ImageFormat::Png => {
            png_envelope(&bytes, &check)?;
            let decoder =
                image::codecs::png::PngDecoder::with_limits(Cursor::new(&bytes), limits())?;
            let (width, height) = dimensions(decoder.dimensions())?;
            let animated = decoder.is_apng()?;
            let frames = if animated {
                still(decoder)?;
                check()?;
                let decoder =
                    image::codecs::png::PngDecoder::with_limits(Cursor::new(&bytes), limits())?;
                animation(decoder.apng()?.into_frames(), &check)?
            } else {
                still(decoder)?;
                1
            };
            (width, height, frames)
        }
        ImageFormat::Jpeg => {
            ensure!(
                bytes.ends_with(&[0xff, 0xd9]),
                "Artwork JPEG is missing its end marker"
            );
            let mut decoder = image::codecs::jpeg::JpegDecoder::new(Cursor::new(&bytes))?;
            decoder.set_limits(limits())?;
            let (width, height) = dimensions(decoder.dimensions())?;
            still(decoder)?;
            (width, height, 1)
        }
        ImageFormat::Gif => {
            ensure!(
                bytes.last() == Some(&0x3b),
                "Artwork GIF is missing its end marker"
            );
            let mut decoder = image::codecs::gif::GifDecoder::new(Cursor::new(&bytes))?;
            decoder.set_limits(limits())?;
            let (width, height) = dimensions(decoder.dimensions())?;
            let frames = animation(decoder.into_frames(), &check)?;
            (width, height, frames)
        }
        ImageFormat::WebP => {
            ensure!(bytes.len() >= 12, "Artwork WebP is truncated");
            let length = u32::from_le_bytes(bytes[4..8].try_into()?) as usize;
            ensure!(
                length.checked_add(8) == Some(bytes.len()),
                "Artwork WebP container size is invalid"
            );
            let mut decoder = image::codecs::webp::WebPDecoder::new(Cursor::new(&bytes))?;
            decoder.set_limits(limits())?;
            let (width, height) = dimensions(decoder.dimensions())?;
            let frames = if decoder.has_animation() {
                animation(decoder.into_frames(), &check)?
            } else {
                still(decoder)?;
                1
            };
            (width, height, frames)
        }
        _ => unreachable!(),
    };
    check()?;
    let metadata = ArtworkBlobMeta {
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        mime_type: mime.into(),
        width,
        height,
        frames,
        encoded_bytes: bytes.len() as u64,
    };
    metadata.validate()?;
    Ok(VerifiedArtwork {
        metadata,
        encoded: bytes.into(),
    })
}
