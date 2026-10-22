//! Artwork for rows, cards and the player. Real covers come first: embedded
//! art of local files, artwork of list members and — once the user allowed
//! online covers — downloaded images. Everything else gets a drawn cover:
//! a muted tone picked from the title, the title's first character set
//! large, and the title itself on bigger sizes, so a library without art
//! still reads as a designed shelf instead of a wall of placeholders.

use crate::{
    app::State,
    art::ArtImage,
    bridge::{Job, Store, image_key},
    paint::P,
    rows::{Row, RowSource},
    theme::with_alpha,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, mpsc},
};
use zsui::{HorizontalAlign, TextWeight};

#[derive(Clone)]
pub struct Covers {
    store: Arc<Mutex<Store>>,
    live: Arc<Mutex<HashMap<String, Arc<ArtImage>>>>,
    jobs: mpsc::Sender<Job>,
}

impl Covers {
    pub fn new(state: &State) -> Self {
        Covers {
            store: state.bridge.shared_store(),
            live: Arc::new(Mutex::new(state.bridge.store().art.live_images())),
            jobs: state.bridge.sender(),
        }
    }

    /// Keep the previous visible set until this layout has installed its new set.
    pub fn begin_frame(&self, cx: &zsui::ZsCanvasLayoutContext<'_>) -> CoverFrame {
        // Natural-height measurement precedes painting and has no visible set.
        if cx.size().height.0 <= 0.0 {
            return CoverFrame {
                _previous: HashMap::new(),
            };
        }
        CoverFrame {
            _previous: std::mem::take(&mut *self.live.lock().unwrap_or_else(|e| e.into_inner())),
        }
    }

    fn pin(&self, key: &str, image: Arc<ArtImage>) -> Arc<ArtImage> {
        self.live
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), image.clone());
        image
    }

    /// The artwork of a list member, requested the first time it is wanted.
    pub fn get(&self, key: &str, list_id: &str, local: bool) -> Option<Arc<ArtImage>> {
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(image) = store.art.image(key) {
            return Some(self.pin(key, image));
        }
        if store.art.get(key).is_some() {
            return None;
        }
        if local || store.auto_art {
            let _ = self.jobs.send(Job::ResolveArt {
                key: key.to_string(),
                list_id: list_id.to_string(),
                music_id: key.to_string(),
            });
        }
        None
    }

    /// A downloaded cover, fetched the first time it is wanted.
    pub fn url(&self, url: Option<&str>) -> Option<Arc<ArtImage>> {
        let url = url?.trim();
        if url.is_empty() {
            return None;
        }
        let key = image_key(url);
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(image) = store.art.image(&key) {
            return Some(self.pin(&key, image));
        }
        if store.art.get(&key).is_some() {
            return None;
        }
        if store.auto_art {
            let _ = self.jobs.send(Job::FetchImage(url.to_string()));
        }
        None
    }

    /// The artwork of a table row, by where the row comes from.
    pub fn row(&self, row: &Row, source: &RowSource) -> Option<Arc<ArtImage>> {
        let local = row.source == "local";
        match source {
            RowSource::Catalog => self.get(&row.id, "catalog", true),
            RowSource::List(id) => self
                .get(&row.id, id, local)
                .or_else(|| self.url(row.image.as_deref())),
            RowSource::Temp(_) if local => self.get(&row.id, "catalog", true),
            // Search, chart and playlist results are not list members; they
            // can only show the cover address they carry.
            RowSource::Temp(_) => self.url(row.image.as_deref()),
        }
    }

    /// The artwork of a catalog track, such as the first track of an album.
    pub fn track(&self, id: &str) -> Option<Arc<ArtImage>> {
        if id.is_empty() {
            return None;
        }
        self.get(id, "catalog", true)
    }
}

pub struct CoverFrame {
    _previous: HashMap<String, Arc<ArtImage>>,
}

/// Covers are requested only for rows intersecting the current viewport.
pub fn visible(cx: &zsui::ZsCanvasLayoutContext<'_>, y: f32, height: f32) -> bool {
    let rect = cx.visible_rect();
    y + height >= rect.y.0 && y <= rect.y.0 + rect.height.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bridge::ArtState;

    #[test]
    fn retained_views_see_new_cache_entries_and_do_not_pin_old_covers() {
        let store = Arc::new(Mutex::new(Store::default()));
        let (jobs, requests) = mpsc::channel();
        let covers = Covers {
            store: store.clone(),
            jobs,
            live: Arc::new(Mutex::new(HashMap::new())),
        };
        assert!(covers.track("first").is_none());
        assert!(matches!(requests.try_recv(), Ok(Job::ResolveArt { .. })));
        let image = Arc::new(
            crate::art::decode(include_bytes!("../../../assets/lx-native-64.png")).unwrap(),
        );
        let weak = Arc::downgrade(&image);
        store
            .lock()
            .unwrap()
            .art
            .insert("first".into(), ArtState::Ready(image.clone()));
        let live = covers.track("first").unwrap();
        assert!(Arc::ptr_eq(&image, &live));
        store.lock().unwrap().art.remove("first");
        drop(image);
        drop(live);
        let measure = zsui::ZsCanvasLayoutContext::detached(zsui::ZsCanvasSize::new(
            zsui::Dp::new(800.0),
            zsui::Dp::new(0.0),
        ));
        drop(covers.begin_frame(&measure));
        assert!(weak.upgrade().is_some());
        let paint = zsui::ZsCanvasLayoutContext::detached(zsui::ZsCanvasSize::new(
            zsui::Dp::new(800.0),
            zsui::Dp::new(600.0),
        ));
        drop(covers.begin_frame(&paint));
        assert!(weak.upgrade().is_none());
        assert!(covers.track("first").is_none());
    }
}

/// The character a drawn cover shows: the first letter of the title,
/// skipping brackets and punctuation; Latin letters in capitals.
pub fn monogram(title: &str) -> String {
    title
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().collect::<String>())
        .unwrap_or_else(|| "♪".into())
}

/// Draws a cover: the artwork, or a drawn cover for `title`.
#[allow(clippy::too_many_arguments)]
pub fn draw(
    p: &mut P,
    art: Option<&ArtImage>,
    x: f32,
    y: f32,
    size: f32,
    radius: f32,
    scale: f32,
    title: &str,
) {
    match art {
        Some(image) => image_at(p, image, x, y, size, radius, scale),
        None => drawn(p, x, y, size, radius, title, None),
    }
}

/// Draws a cover image at exactly `size` pixels.
pub fn image_at(p: &mut P, image: &ArtImage, x: f32, y: f32, size: f32, radius: f32, scale: f32) {
    let frame = image.frame(
        (size * scale).round() as u32,
        (radius * scale).round() as u32,
    );
    p.image(&frame, x, y, size, size);
}

/// A drawn cover: a tone from `title`, its monogram, and on larger sizes
/// the title set small in the corner. `glyph` replaces the monogram.
pub fn drawn(p: &mut P, x: f32, y: f32, size: f32, radius: f32, title: &str, glyph: Option<char>) {
    let pal = p.pal;
    let tone = pal.tone(title);
    p.round(x, y, size, size, radius, tone);
    let ink = pal.tone_ink;
    if let Some(glyph) = glyph {
        p.glyph(
            glyph,
            x,
            y,
            size,
            size,
            (size * 0.34).clamp(9.0, 96.0),
            with_alpha(ink, if pal.dark { 0xa8 } else { 0x99 }),
        );
        return;
    }
    let mark = monogram(title);
    if size < 72.0 {
        p.text_ex(
            &mark,
            x,
            y,
            size,
            size,
            (size * 0.44).max(9.0),
            with_alpha(ink, if pal.dark { 0x9c } else { 0x8a }),
            HorizontalAlign::Center,
            TextWeight::Regular,
        );
        return;
    }
    // The monogram large in the lower left, the title quiet above it.
    let big = size * 0.52;
    p.text_ex(
        &mark,
        x + size * 0.08,
        y + size - big * 1.2,
        size * 0.9,
        big * 1.1,
        big,
        with_alpha(ink, if pal.dark { 0x2a } else { 0x24 }),
        HorizontalAlign::Start,
        TextWeight::Regular,
    );
    // The title only on covers large enough to be read alone: cards and
    // rows print it underneath.
    if size >= 200.0 {
        let fs = (size * 0.06).clamp(12.0, 18.0);
        p.text_ex(
            title,
            x + size * 0.09,
            y + size * 0.08,
            size * 0.82,
            fs * 1.6,
            fs,
            with_alpha(ink, if pal.dark { 0xd8 } else { 0xcc }),
            HorizontalAlign::Start,
            TextWeight::Semibold,
        );
        p.fill(
            x + size * 0.09,
            y + size * 0.08 + fs * 1.75,
            (size * 0.12).max(12.0),
            1.5,
            with_alpha(ink, 0x66),
        );
    }
}

/// A round artist portrait: the artwork, or the name's first letter.
pub fn avatar(
    p: &mut P,
    art: Option<&ArtImage>,
    x: f32,
    y: f32,
    size: f32,
    scale: f32,
    name: &str,
) {
    match art {
        Some(image) => image_at(p, image, x, y, size, size / 2.0, scale),
        None => {
            let pal = p.pal;
            p.round(x, y, size, size, size / 2.0, pal.tone(name));
            p.text_ex(
                &monogram(name),
                x,
                y,
                size,
                size,
                size * 0.38,
                with_alpha(pal.tone_ink, if pal.dark { 0xb0 } else { 0x99 }),
                HorizontalAlign::Center,
                TextWeight::Regular,
            );
        }
    }
}

/// A playlist cover from its first tracks: four covers in a square when
/// there are four, the first one alone otherwise, a drawn cover without.
#[allow(clippy::too_many_arguments)]
pub fn mosaic(
    p: &mut P,
    arts: &[Arc<ArtImage>],
    x: f32,
    y: f32,
    size: f32,
    radius: f32,
    scale: f32,
    title: &str,
    background: zsui::Color,
) {
    if arts.len() >= 4 && size >= 64.0 {
        let half = size / 2.0;
        for (i, image) in arts.iter().take(4).enumerate() {
            let (cx, cy) = (x + (i % 2) as f32 * half, y + (i / 2) as f32 * half);
            image_at(p, image, cx, cy, half, 0.0, scale);
        }
        round_corners(p, x, y, size, radius, background);
        return;
    }
    match arts.first() {
        Some(image) => image_at(p, image, x, y, size, radius, scale),
        None => drawn(p, x, y, size, radius, title, None),
    }
}

/// Rounds the corners of a square drawn without them by painting the
/// corner outside a quarter circle in the background color.
pub fn round_corners(p: &mut P, x: f32, y: f32, size: f32, radius: f32, background: zsui::Color) {
    use zsui::{Dp, NativeDrawFill, ZsCanvasPoint, ZsCanvasPrimitive};
    if radius <= 0.5 {
        return;
    }
    let point = |px: f32, py: f32| ZsCanvasPoint::new(Dp::new(px), Dp::new(py));
    // (corner, arc center, start angle) for each corner.
    let corners = [
        ((x, y), (x + radius, y + radius), 180.0f32),
        ((x + size, y), (x + size - radius, y + radius), 270.0),
        (
            (x + size, y + size),
            (x + size - radius, y + size - radius),
            0.0,
        ),
        ((x, y + size), (x + radius, y + size - radius), 90.0),
    ];
    for ((cx, cy), (ax, ay), start) in corners {
        let mut points = vec![point(cx, cy)];
        for step in 0..=8 {
            let angle = (start + step as f32 * 90.0 / 8.0).to_radians();
            points.push(point(ax + radius * angle.cos(), ay + radius * angle.sin()));
        }
        p.scene.push(ZsCanvasPrimitive::polygon(
            points,
            NativeDrawFill::Color(background),
        ));
    }
}
