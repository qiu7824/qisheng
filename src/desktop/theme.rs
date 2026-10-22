//! Palette and icon glyphs.
//!
//! The interface is monochrome: neutral greys in two themes, type in three
//! steps of contrast, and primary buttons drawn in the text color. Color
//! comes from album art only — the player panel and the full-screen stage
//! take a tint from the playing cover — plus a rose heart for favorites.
//! Missing covers are drawn from a set of muted tones (see `covers`).
//!
//! When the system high-contrast mode is active ZSUI ignores these colors
//! and falls back to the semantic role chosen by `role_for`.

use std::sync::{
    OnceLock,
    atomic::{AtomicU8, Ordering},
};
use zsui::{Color, ColorRole, ZsuiThemeMode};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    pub dark: bool,
    /// The page area.
    pub surface: Color,
    /// The navigation column.
    pub nav: Color,
    /// The player panel.
    pub panel: Color,
    /// Fields, chips and quiet buttons.
    pub raised: Color,
    pub hover: Color,
    /// Selected rows and pressed buttons.
    pub pressed: Color,
    /// Hairlines between regions and rows.
    pub line: Color,
    pub line_strong: Color,
    pub text: Color,
    pub text2: Color,
    pub text3: Color,
    /// Primary buttons are drawn in the text color.
    pub control: Color,
    pub on_control: Color,
    /// Favorites.
    pub love: Color,
    pub danger: Color,
    pub ok: Color,
    pub warn: Color,
    pub close_hover: Color,
    pub white: Color,
    pub shadow: Color,
    /// Dims the window under dialogs.
    pub scrim: Color,
    /// Backgrounds of drawn covers, and the type on them.
    pub tones: [Color; 8],
    pub tone_ink: Color,
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::rgb(r, g, b)
}

impl Palette {
    pub const fn dark() -> Self {
        Self {
            dark: true,
            surface: rgb(0x13, 0x13, 0x15),
            nav: rgb(0x0d, 0x0d, 0x0f),
            panel: rgb(0x18, 0x18, 0x1b),
            raised: rgb(0x20, 0x20, 0x24),
            hover: rgb(0x1d, 0x1d, 0x21),
            pressed: rgb(0x27, 0x27, 0x2c),
            line: rgb(0x24, 0x24, 0x29),
            line_strong: rgb(0x35, 0x35, 0x3b),
            text: rgb(0xf2, 0xf2, 0xf4),
            text2: rgb(0xa4, 0xa4, 0xad),
            text3: rgb(0x6c, 0x6c, 0x76),
            control: rgb(0xf2, 0xf2, 0xf4),
            on_control: rgb(0x13, 0x13, 0x15),
            love: rgb(0xf2, 0x5f, 0x72),
            danger: rgb(0xee, 0x6a, 0x64),
            ok: rgb(0x52, 0xb7, 0x88),
            warn: rgb(0xd9, 0xa4, 0x4a),
            close_hover: rgb(0xc4, 0x2b, 0x1c),
            white: rgb(0xff, 0xff, 0xff),
            shadow: Color::rgba(0, 0, 0, 0x70),
            scrim: Color::rgba(0, 0, 0, 0x99),
            tones: [
                rgb(0x26, 0x2b, 0x33),
                rgb(0x27, 0x2e, 0x2a),
                rgb(0x2e, 0x28, 0x32),
                rgb(0x33, 0x2a, 0x28),
                rgb(0x30, 0x2d, 0x26),
                rgb(0x23, 0x29, 0x31),
                rgb(0x29, 0x31, 0x2f),
                rgb(0x31, 0x29, 0x2e),
            ],
            tone_ink: rgb(0xf2, 0xf2, 0xf4),
        }
    }

    pub const fn light() -> Self {
        Self {
            dark: false,
            surface: rgb(0xff, 0xff, 0xff),
            nav: rgb(0xf5, 0xf5, 0xf7),
            panel: rgb(0xf8, 0xf8, 0xfa),
            raised: rgb(0xf0, 0xf0, 0xf3),
            hover: rgb(0xf3, 0xf3, 0xf6),
            pressed: rgb(0xe8, 0xe8, 0xec),
            line: rgb(0xec, 0xec, 0xf0),
            line_strong: rgb(0xdb, 0xdb, 0xe1),
            text: rgb(0x14, 0x14, 0x17),
            text2: rgb(0x5b, 0x5b, 0x66),
            text3: rgb(0x98, 0x98, 0xa3),
            control: rgb(0x14, 0x14, 0x17),
            on_control: rgb(0xff, 0xff, 0xff),
            love: rgb(0xe0, 0x42, 0x5c),
            danger: rgb(0xd2, 0x3f, 0x3a),
            ok: rgb(0x2e, 0x93, 0x63),
            warn: rgb(0xb4, 0x7b, 0x1c),
            close_hover: rgb(0xc4, 0x2b, 0x1c),
            white: rgb(0xff, 0xff, 0xff),
            shadow: Color::rgba(0x14, 0x14, 0x30, 0x2a),
            scrim: Color::rgba(0x10, 0x10, 0x14, 0x66),
            tones: [
                rgb(0xe3, 0xe7, 0xee),
                rgb(0xe2, 0xea, 0xe5),
                rgb(0xea, 0xe4, 0xee),
                rgb(0xee, 0xe5, 0xe2),
                rgb(0xed, 0xe9, 0xe0),
                rgb(0xe0, 0xe6, 0xee),
                rgb(0xe1, 0xec, 0xe8),
                rgb(0xec, 0xe3, 0xea),
            ],
            tone_ink: rgb(0x14, 0x14, 0x17),
        }
    }

    /// The palette of the current theme mode.
    pub fn get() -> Self {
        match theme_mode() {
            ZsuiThemeMode::Light => Self::light(),
            _ => Self::dark(),
        }
    }

    /// The drawn-cover tone for `seed`, stable across runs.
    pub fn tone(&self, seed: &str) -> Color {
        // FNV-1a: stable, unlike the standard hasher.
        let mut hash: u32 = 0x811c_9dc5;
        for byte in seed.bytes() {
            hash ^= u32::from(byte);
            hash = hash.wrapping_mul(0x0100_0193);
        }
        self.tones[(hash % self.tones.len() as u32) as usize]
    }

    /// Semantic fallback for a brand color under system high contrast.
    pub fn role_for(&self, color: Color) -> ColorRole {
        if color == self.text || color == self.white || color == self.tone_ink {
            ColorRole::PrimaryText
        } else if color == self.love {
            ColorRole::Accent
        } else if color == self.on_control {
            ColorRole::AccentText
        } else if color == self.danger || color == self.close_hover {
            ColorRole::Danger
        } else if color == self.ok {
            ColorRole::Success
        } else {
            ColorRole::SecondaryText
        }
    }
}

/// The current mode: 1 light, 2 dark, 0 before the first read.
static MODE: AtomicU8 = AtomicU8::new(0);

/// `LX_THEME=dark|light` pins the theme; otherwise it follows the system.
fn pinned_mode() -> Option<ZsuiThemeMode> {
    static PINNED: OnceLock<Option<ZsuiThemeMode>> = OnceLock::new();
    *PINNED.get_or_init(|| {
        match std::env::var("LX_THEME")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "dark" => Some(ZsuiThemeMode::Dark),
            "light" => Some(ZsuiThemeMode::Light),
            _ => None,
        }
    })
}

fn store_mode(mode: ZsuiThemeMode) -> ZsuiThemeMode {
    let mode = match mode {
        ZsuiThemeMode::Light => ZsuiThemeMode::Light,
        // The player is designed dark-first; other modes use it.
        _ => ZsuiThemeMode::Dark,
    };
    MODE.store(
        if mode == ZsuiThemeMode::Light { 1 } else { 2 },
        Ordering::Relaxed,
    );
    mode
}

/// The theme mode ZSUI controls (text inputs, menus) should use so they sit
/// on the same light or dark surfaces as the custom-drawn content.
pub fn theme_mode() -> ZsuiThemeMode {
    match MODE.load(Ordering::Relaxed) {
        1 => ZsuiThemeMode::Light,
        2 => ZsuiThemeMode::Dark,
        _ => {
            #[cfg(windows)]
            let system = zsui::windows_system_theme_mode();
            #[cfg(not(windows))]
            let system = ZsuiThemeMode::Dark;
            store_mode(pinned_mode().unwrap_or(system))
        }
    }
}

/// Follows a system theme change unless `LX_THEME` pins the theme; reports
/// whether the palette changed.
pub fn follow_system_theme(mode: ZsuiThemeMode) -> bool {
    if pinned_mode().is_some() {
        return false;
    }
    let before = theme_mode();
    store_mode(mode) != before
}

/// Mixes `a` towards `b`; `t` = 0 keeps `a`, 1 gives `b`.
pub fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    Color::rgb(ch(a.r, b.r), ch(a.g, b.g), ch(a.b, b.b))
}

pub fn with_alpha(color: Color, alpha: u8) -> Color {
    Color::rgba(color.r, color.g, color.b, alpha)
}

/// A cover tone for the panel and stage: the art's color with its
/// lightness pulled to suit the theme, so tints stay quiet.
pub fn tint(tone: Color, dark: bool) -> Color {
    let (r, g, b) = (tone.r as f32, tone.g as f32, tone.b as f32);
    let max = r.max(g).max(b).max(1.0);
    let target = if dark { 72.0 } else { 236.0 };
    if dark {
        let k = target / max;
        Color::rgb((r * k) as u8, (g * k) as u8, (b * k) as u8)
    } else {
        // Light themes keep a pale wash of the hue.
        let t = 0.62;
        Color::rgb(
            (r + (target + 19.0 - r) * t).min(255.0) as u8,
            (g + (target + 19.0 - g) * t).min(255.0) as u8,
            (b + (target + 19.0 - b) * t).min(255.0) as u8,
        )
    }
}

/// Bahnschrift: figures for times, counts and ranks.
pub const NUM_FONT: &str = "Bahnschrift";

/// Segoe MDL2 Assets code points available on every supported Windows.
pub mod glyph {
    pub const PLAY: char = '\u{F5B0}';
    pub const PAUSE: char = '\u{F8AE}';
    pub const PREVIOUS: char = '\u{F8AC}';
    pub const NEXT: char = '\u{F8AD}';
    pub const SHUFFLE: char = '\u{E8B1}';
    pub const REPEAT_ALL: char = '\u{E8EE}';
    pub const REPEAT_ONE: char = '\u{E8ED}';
    pub const ORDER: char = '\u{E8AB}';
    pub const STOP_AFTER: char = '\u{E7E8}';
    pub const VOLUME: char = '\u{E995}';
    pub const VOLUME0: char = '\u{E993}';
    pub const VOLUME1: char = '\u{E994}';
    pub const MUTE: char = '\u{E74F}';
    pub const HEART: char = '\u{EB51}';
    pub const HEART_FILL: char = '\u{EB52}';
    pub const QUEUE: char = '\u{E142}';
    pub const NOTE: char = '\u{EC4F}';
    pub const NOTES: char = '\u{E8D6}';
    pub const SEARCH: char = '\u{E721}';
    pub const FOLDER: char = '\u{ED25}';
    pub const SETTINGS: char = '\u{E713}';
    pub const SOURCE: char = '\u{EA86}';
    pub const ADD: char = '\u{E710}';
    pub const MORE: char = '\u{E712}';
    pub const CANCEL: char = '\u{E711}';
    pub const CHECK: char = '\u{E73E}';
    pub const INFO: char = '\u{E946}';
    pub const WARNING: char = '\u{E7BA}';
    pub const SHIELD: char = '\u{EA18}';
    pub const MIN: char = '\u{E921}';
    pub const MAX: char = '\u{E922}';
    pub const RESTORE: char = '\u{E923}';
    pub const CLOSE: char = '\u{E8BB}';
    pub const TIMER: char = '\u{E916}';
    pub const CHEVRON_DOWN: char = '\u{E70D}';
    pub const CHEVRON_LEFT: char = '\u{E76B}';
    pub const CHEVRON_RIGHT: char = '\u{E76C}';
    pub const HOME: char = '\u{E80F}';
    pub const HOME_FILL: char = '\u{EA8A}';
    pub const GRID: char = '\u{F0E2}';
    pub const CHART: char = '\u{E1E9}';
    pub const ALBUM: char = '\u{E93C}';
    pub const ARTIST: char = '\u{E77B}';
    pub const DISC: char = '\u{E95A}';
    pub const GLOBE: char = '\u{E774}';
    pub const EXPAND: char = '\u{E740}';
}
