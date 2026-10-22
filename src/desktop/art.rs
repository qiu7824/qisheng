//! Cover art: decoding and exact-pixel frames with rounded corners.

use crate::art_cache::ByteLru;
use image::{GenericImageView, ImageDecoder, RgbaImage, imageops::FilterType};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};
use zsui::{Color, ZsImageFrame, ZsImageFrameId};

const MASTER: u32 = 480;
/// Online thumbnails keep a smaller master; they are only drawn small.
const THUMB_MASTER: u32 = 300;
pub const MASTER_CACHE_BYTES: usize = 12 * 1024 * 1024;
const FRAME_CACHE_BYTES: usize = 8 * 1024 * 1024;
const PNG_CACHE_BYTES: usize = 512 * 1024;
type FrameKey = (u64, u32, u32, bool);

fn frames() -> &'static Mutex<ByteLru<FrameKey, ZsImageFrame>> {
    static CACHE: OnceLock<Mutex<ByteLru<FrameKey, ZsImageFrame>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ByteLru::new(FRAME_CACHE_BYTES, 256)))
}

fn pngs() -> &'static Mutex<ByteLru<u64, Arc<Vec<u8>>>> {
    static CACHE: OnceLock<Mutex<ByteLru<u64, Arc<Vec<u8>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(ByteLru::new(PNG_CACHE_BYTES, 4)))
}

fn next_id() -> ZsImageFrameId {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    ZsImageFrameId::new(NEXT.fetch_add(1, Ordering::Relaxed))
}

pub struct ArtImage {
    id: u64,
    master: RgbaImage,
    /// Average vivid color, used to tint the now-playing view.
    pub tone: Color,
}

impl std::fmt::Debug for ArtImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArtImage")
            .field("tone", &self.tone)
            .finish()
    }
}

pub fn decode(bytes: &[u8]) -> Result<ArtImage, String> {
    decode_at(bytes, MASTER)
}

/// Decodes a downloaded thumbnail, such as a playlist cover.
pub fn decode_thumb(bytes: &[u8]) -> Result<ArtImage, String> {
    decode_at(bytes, THUMB_MASTER)
}

fn decode_at(bytes: &[u8], master_px: u32) -> Result<ArtImage, String> {
    // Serialize full-resolution decoding across URL and embedded artwork.
    static DECODE: Mutex<()> = Mutex::new(());
    let _decode = DECODE.lock().unwrap_or_else(|e| e.into_inner());
    let mut reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    if decoder.total_bytes() > 64 * 1024 * 1024 {
        return Err("cover exceeds the decoded pixel budget".into());
    }
    let image = image::DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())?;
    let (w, h) = (image.width(), image.height());
    if w == 0 || h == 0 {
        return Err("empty image".into());
    }
    // Center-crop to a square, then keep one bounded master copy.
    let side = w.min(h);
    // Resize a borrowed crop: no second full-resolution square allocation.
    let cropped = image.view((w - side) / 2, (h - side) / 2, side, side);
    let master = image::imageops::resize(
        &*cropped,
        master_px.min(side),
        master_px.min(side),
        FilterType::CatmullRom,
    );
    static NEXT_IMAGE: AtomicU64 = AtomicU64::new(1);
    Ok(ArtImage {
        id: NEXT_IMAGE.fetch_add(1, Ordering::Relaxed),
        tone: tone_of(&master),
        master,
    })
}

impl ArtImage {
    pub fn decoded_bytes(&self) -> usize {
        self.master.as_raw().capacity()
    }
    /// A 300-pixel PNG for the system media overlay, shared in a bounded cache.
    pub fn png(&self) -> Option<Arc<Vec<u8>>> {
        let mut cache = pngs().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(png) = cache.get(&self.id) {
            return Some(png.clone());
        }
        let side = self.master.width().min(300);
        let scaled = image::imageops::resize(&self.master, side, side, FilterType::Triangle);
        let mut bytes = std::io::Cursor::new(Vec::new());
        scaled.write_to(&mut bytes, image::ImageFormat::Png).ok()?;
        let png = Arc::new(bytes.into_inner());
        cache.insert(self.id, png.clone(), png.capacity());
        Some(png)
    }

    /// A heavily blurred, quieted copy of the cover for the full-screen
    /// stage, cropped to `aspect` (width / height). It is tiny and meant to
    /// be scaled up smoothly, which finishes the blur.
    pub fn backdrop(&self, aspect: f32, dark: bool) -> ZsImageFrame {
        let bucket = (aspect.clamp(0.5, 3.0) * 10.0).round() as u32;
        let mut cache = frames().lock().unwrap_or_else(|e| e.into_inner());
        let key = (self.id, bucket, u32::from(dark), true);
        if let Some(frame) = cache.get(&key) {
            return frame.clone();
        }
        let aspect = bucket as f32 / 10.0;
        let small = image::imageops::resize(&self.master, 48, 48, FilterType::Triangle);
        let blurred = image::imageops::blur(&small, 3.2);
        let (w, h) = if aspect >= 1.0 {
            (40u32, ((40.0 / aspect).round() as u32).clamp(8, 40))
        } else {
            (((40.0 * aspect).round() as u32).clamp(8, 40), 40u32)
        };
        let x = (48 - w) / 2;
        let y = (48 - h) / 2;
        let mut crop = image::imageops::crop_imm(&blurred, x, y, w, h).to_image();
        for pixel in crop.pixels_mut() {
            let [r, g, b, _] = pixel.0.map(|c| c as f32);
            let grey = r * 0.3 + g * 0.59 + b * 0.11;
            // Keep most of the hue but less of its saturation.
            let mut c = [r, g, b].map(|v| v + (grey - v) * 0.3);
            if dark {
                c = c.map(|v| v * 0.42);
            } else {
                c = c.map(|v| v + (255.0 - v) * 0.62);
            }
            pixel.0 = [c[0] as u8, c[1] as u8, c[2] as u8, 255];
        }
        let frame =
            ZsImageFrame::from_rgba8(next_id(), w, h, crop.into_raw()).expect("backdrop frame");
        cache.insert(key, frame.clone(), frame.decoded_bytes());
        frame
    }

    /// A rounded frame capped at source resolution; the renderer scales to layout size.
    pub fn frame(&self, px: u32, radius_px: u32) -> ZsImageFrame {
        let requested = px.clamp(8, 4096);
        let px = requested.min(self.master.width());
        let radius_px =
            ((radius_px as u64 * px as u64) / requested as u64).min((px / 2) as u64) as u32;
        let mut cache = frames().lock().unwrap_or_else(|e| e.into_inner());
        let key = (self.id, px, radius_px, false);
        if let Some(frame) = cache.get(&key) {
            return frame.clone();
        }
        let mut scaled = image::imageops::resize(&self.master, px, px, FilterType::CatmullRom);
        round_corners(&mut scaled, radius_px as f32);
        let frame = ZsImageFrame::from_rgba8(next_id(), px, px, scaled.into_raw())
            .expect("square cover frame");
        cache.insert(key, frame.clone(), frame.decoded_bytes());
        frame
    }
}

/// The application icon at `px` pixels, for the in-window brand mark.
pub fn logo(px: u32) -> Option<ZsImageFrame> {
    static MASTER: OnceLock<Option<RgbaImage>> = OnceLock::new();
    static FRAMES: OnceLock<Mutex<ByteLru<u32, ZsImageFrame>>> = OnceLock::new();
    let master = MASTER
        .get_or_init(|| {
            image::load_from_memory(include_bytes!("../../assets/lx-native-64.png"))
                .ok()
                .map(|image| image.to_rgba8())
        })
        .as_ref()?;
    let px = px.clamp(8, 128);
    let frames = FRAMES.get_or_init(|| Mutex::new(ByteLru::new(256 * 1024, 16)));
    let mut frames = frames.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(frame) = frames.get(&px) {
        return Some(frame.clone());
    }
    let scaled = image::imageops::resize(master, px, px, FilterType::CatmullRom);
    let frame = ZsImageFrame::from_rgba8(next_id(), px, px, scaled.into_raw()).ok()?;
    frames.insert(px, frame.clone(), frame.decoded_bytes());
    Some(frame)
}

/// Anti-aliased rounded-corner alpha mask.
fn round_corners(image: &mut RgbaImage, radius: f32) {
    if radius <= 0.5 {
        return;
    }
    let (w, h) = (image.width() as f32, image.height() as f32);
    let r = radius.min(w / 2.0).min(h / 2.0);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
        let cx = if px < r {
            r
        } else if px > w - r {
            w - r
        } else {
            continue;
        };
        let cy = if py < r {
            r
        } else if py > h - r {
            h - r
        } else {
            continue;
        };
        let distance = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
        let coverage = (r + 0.5 - distance).clamp(0.0, 1.0);
        pixel[3] = (pixel[3] as f32 * coverage).round() as u8;
    }
}

fn tone_of(image: &RgbaImage) -> Color {
    let small = image::imageops::resize(image, 16, 16, FilterType::Triangle);
    let (mut r, mut g, mut b, mut weight) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
    for pixel in small.pixels() {
        let [pr, pg, pb, _] = pixel.0.map(|c| c as f32 / 255.0);
        let max = pr.max(pg).max(pb);
        let min = pr.min(pg).min(pb);
        // Prefer saturated, mid-bright pixels.
        let w = (max - min) * (1.0 - (max - 0.6).abs()) + 0.02;
        r += pr * w;
        g += pg * w;
        b += pb * w;
        weight += w;
    }
    let to = |v: f32| ((v / weight).clamp(0.0, 1.0) * 255.0) as u8;
    Color::rgb(to(r), to(g), to(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory_sample(phase: &str) {
        #[cfg(windows)]
        {
            #[repr(C)]
            #[derive(Default)]
            struct Counters {
                cb: u32,
                faults: u32,
                peak_working_set: usize,
                working_set: usize,
                peak_paged: usize,
                paged: usize,
                peak_nonpaged: usize,
                nonpaged: usize,
                pagefile: usize,
                peak_pagefile: usize,
                private: usize,
            }
            #[link(name = "kernel32")]
            unsafe extern "system" {
                fn K32GetProcessMemoryInfo(
                    process: *mut std::ffi::c_void,
                    counters: *mut Counters,
                    size: u32,
                ) -> i32;
            }
            let mut counters = Counters {
                cb: std::mem::size_of::<Counters>() as u32,
                ..Counters::default()
            };
            let ok = unsafe {
                K32GetProcessMemoryInfo(
                    windows_sys::Win32::System::Threading::GetCurrentProcess(),
                    &mut counters,
                    std::mem::size_of::<Counters>() as u32,
                )
            };
            assert_ne!(ok, 0);
            println!(
                "HEADLESS_COVER_MEMORY {}",
                serde_json::json!({
                    "phase": phase, "working_set_bytes": counters.working_set,
                    "peak_working_set_bytes": counters.peak_working_set,
                    "private_commit_bytes": counters.private, "peak_private_commit_bytes": counters.peak_pagefile,
                    "scope": "headless cover-cache test process; not the GUI application",
                })
            );
        }
        #[cfg(not(windows))]
        let _ = phase;
    }

    pub fn png_fixture(w: u32, h: u32) -> Vec<u8> {
        let pixels = RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x % 251) as u8, (y % 241) as u8, ((x + y) % 239) as u8, 255])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        pixels.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn rejects_oversized_or_incomplete_images_before_retaining_pixels() {
        assert!(decode_thumb(&png_fixture(4097, 1)).is_err());
        let valid = png_fixture(300, 200);
        assert!(decode_thumb(&valid[..valid.len() / 2]).is_err());
        let image = decode_thumb(&valid).unwrap();
        assert_eq!(image.decoded_bytes(), 200 * 200 * 4);
    }

    #[test]
    fn frames_reuse_pixels_and_do_not_upscale_the_cache() {
        let image = decode_thumb(&png_fixture(300, 300)).unwrap();
        let a = image.frame(900, 30);
        let b = image.frame(900, 30);
        assert_eq!(a, b);
        assert_eq!(a.decoded_bytes(), 300 * 300 * 4);
        let png = image.png().unwrap();
        assert!(Arc::ptr_eq(&png, &image.png().unwrap()));
        let decoded = image::load_from_memory(&png).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (300, 300));
    }

    #[test]
    fn browsing_and_resizing_hundreds_of_covers_stays_within_cache_budgets() {
        memory_sample("start");
        let mut masters = ByteLru::new(MASTER_CACHE_BYTES, 512);
        let source = png_fixture(300, 300);
        let mut retired = Vec::new();
        for page in 0..3 {
            for cover in 0..240 {
                let key = format!("page-{page}-cover-{cover}");
                let image = Arc::new(decode_thumb(&source).unwrap());
                retired.push(Arc::downgrade(&image));
                for size in [48, 160, 240, 480] {
                    image.frame(size, 12);
                }
                if cover % 20 == 0 {
                    image.png();
                    image.backdrop(1.7, true);
                }
                masters.insert(key, image.clone(), image.decoded_bytes());
                assert!(masters.bytes() <= MASTER_CACHE_BYTES);
                assert!(frames().lock().unwrap().bytes() <= FRAME_CACHE_BYTES);
                assert!(pngs().lock().unwrap().bytes() <= PNG_CACHE_BYTES);
            }
            println!(
                "COVER_CACHE page={page} masters={} frames={} png={} live_masters={}",
                masters.bytes(),
                frames().lock().unwrap().bytes(),
                pngs().lock().unwrap().bytes(),
                retired
                    .iter()
                    .filter(|weak| weak.strong_count() > 0)
                    .count()
            );
            assert!(
                retired
                    .iter()
                    .filter(|weak| weak.strong_count() > 0)
                    .count()
                    <= 35
            );
            memory_sample(&format!("page-{page}"));
        }
        drop(masters);
        assert!(retired.iter().all(|weak| weak.upgrade().is_none()));
        memory_sample("masters-released");
    }
}
