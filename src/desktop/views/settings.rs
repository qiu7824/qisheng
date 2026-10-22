//! Settings: playback, output device, equalizer, speed/pitch, sleep timer,
//! artwork and caches. One scrolling canvas with custom controls.

use super::{PAD, scroller, surface};
use crate::{
    app::{self, DragKind, Msg, State, Surface},
    bridge::Job,
    paint::{P, rid, rsplit},
    theme::{Palette, glyph as g, with_alpha},
};
use lx_gpui_player::{
    backend::BackendCommand,
    domain::{
        audio_output::OutputSelection,
        equalizer::{
            EQUALIZER_FREQUENCIES_HZ, EQUALIZER_MAX_GAIN_DB, EQUALIZER_MIN_GAIN_DB,
            EqualizerPreset, EqualizerSettings,
        },
    },
    playback::timeout_stop::TimeoutStopStatus,
};
use zsui::{Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext};

pub const R_BAND: u32 = 100;
pub const R_RATE: u32 = 101;
pub const R_PITCH: u32 = 102;
const R_QUALITY: u32 = 103;
const R_TOGGLE: u32 = 104;
const R_DEVICE: u32 = 105;
const R_PRESET: u32 = 106;
const R_EQ_RESET: u32 = 107;
const R_RATE_RESET: u32 = 108;
const R_TIMER: u32 = 109;
const R_TIMER_CANCEL: u32 = 110;
const R_CLEAR: u32 = 111;
const R_REFRESH_DEVICES: u32 = 112;
const R_PITCH_RESET: u32 = 113;
const R_FOLDER_REMOVE: u32 = 114;
const R_FOLDER_ADD: u32 = 115;
const R_FOLDER_SCAN: u32 = 116;
const R_API_PORT: u32 = 117;

const T_AUTO_SKIP: u32 = 0;
const T_AUTO_SWITCH: u32 = 1;
const T_CONFIRM_AUDIO: u32 = 2;
const T_MUTE: u32 = 3;
const T_PAUSE_ON_CHANGE: u32 = 4;
const T_PRESERVE_PITCH: u32 = 5;
const T_TIMER_TRACK_END: u32 = 6;
const T_AUTO_ART: u32 = 7;
const T_CLOSE_TO_TRAY: u32 = 8;
const T_NOTIFY_TRACKS: u32 = 9;
const T_OPEN_API: u32 = 10;

const QUALITIES: [(&str, &str); 4] = [
    ("128k", "标准"),
    ("320k", "高品质"),
    ("flac", "无损"),
    ("flac24bit", "Hi-Res"),
];

const TIMERS: [u32; 6] = [10, 15, 30, 45, 60, 90];

pub fn preset_name(preset: EqualizerPreset) -> &'static str {
    match preset {
        EqualizerPreset::Flat => "原声",
        EqualizerPreset::Pop => "流行",
        EqualizerPreset::Dance => "舞曲",
        EqualizerPreset::Rock => "摇滚",
        EqualizerPreset::Classical => "古典",
        EqualizerPreset::Vocal => "人声",
        EqualizerPreset::Slow => "慢歌",
        EqualizerPreset::Electronic => "电子",
        EqualizerPreset::Subwoofer => "重低音",
        EqualizerPreset::Soft => "柔和",
    }
}

pub fn rate_from_t(t: f64) -> f64 {
    ((0.5 + t * 1.5) * 20.0).round() / 20.0
}

fn rate_to_t(rate: f64) -> f64 {
    ((rate - 0.5) / 1.5).clamp(0.0, 1.0)
}

pub fn pitch_from_t(t: f64) -> f64 {
    // Snap to semitones between 0.5 and 1.5.
    let ratio = 0.5 + t;
    let semitones = (12.0 * ratio.log2()).round();
    2f64.powf(semitones / 12.0).clamp(0.5, 1.5)
}

fn pitch_to_t(ratio: f64) -> f64 {
    (ratio - 0.5).clamp(0.0, 1.0)
}

#[derive(Clone)]
struct SettingsView {
    api_enabled: bool,
    api_port: u16,
    api: crate::bridge::ApiView,
    quality: String,
    auto_skip: bool,
    auto_switch: bool,
    confirm_audio: bool,
    devices: Vec<(String, String, bool)>,
    selection: OutputSelection,
    active: Option<String>,
    muted: bool,
    pause_on_change: bool,
    eq: [f64; 10],
    preset: Option<EqualizerPreset>,
    rate: f64,
    preserves_pitch: bool,
    pitch: f64,
    timer: Option<(TimeoutStopStatus, u64)>,
    timer_track_end: bool,
    auto_art: bool,
    data_dir: String,
    drag: Option<DragKind>,
    folders: Vec<String>,
    close_to_tray: bool,
    notify_tracks: bool,
}

fn settings_view(state: &State) -> SettingsView {
    let store = state.bridge.store();
    let snapshot = &store.snapshot;
    let queue = store.queue.clone();
    let mut eq = snapshot.equalizer.desired.gains_db;
    let mut rate = snapshot.playback_processing.desired.playback_rate;
    let mut pitch = snapshot.playback_processing.desired.pitch_ratio;
    let drag = state.ui.drag;
    match drag.map(|d| (d.kind, d.value)) {
        Some((DragKind::Band(i), value)) => eq[i] = (value * 2.0).round() / 2.0,
        Some((DragKind::Rate, value)) => rate = value,
        Some((DragKind::Pitch, value)) => pitch = value,
        _ => {}
    }
    let preset = EqualizerPreset::ALL
        .into_iter()
        .find(|p| p.settings() == EqualizerSettings { gains_db: eq });
    let output = &snapshot.audio_output;
    SettingsView {
        api_enabled: state.ui.api_enabled,
        api_port: state.ui.api_port,
        api: store.api.clone(),
        quality: queue
            .as_ref()
            .map(|q| q.quality.clone())
            .unwrap_or_else(|| "128k".into()),
        auto_skip: queue.as_ref().map(|q| q.auto_skip_on_error).unwrap_or(true),
        auto_switch: queue
            .as_ref()
            .map(|q| q.auto_switch_source)
            .unwrap_or(false),
        confirm_audio: !store.auto_approve_audio,
        devices: output
            .devices
            .iter()
            .map(|d| (d.id.clone(), d.name.clone(), d.is_default))
            .collect(),
        selection: output.preferences.selection.clone(),
        active: output.state.active_device_name.clone(),
        muted: output.preferences.muted,
        pause_on_change: output.preferences.pause_on_device_change,
        eq,
        preset,
        rate,
        preserves_pitch: snapshot.playback_processing.desired.preserves_pitch,
        pitch,
        timer: store.timer.as_ref().and_then(|(timer, at)| {
            let remaining = timer
                .remaining_millis
                .map(|ms| ms.saturating_sub(at.elapsed().as_millis() as u64))
                .unwrap_or(0);
            matches!(
                timer.status,
                TimeoutStopStatus::Armed | TimeoutStopStatus::WaitingForTrackEnd
            )
            .then_some((timer.status, remaining))
        }),
        timer_track_end: state.ui.timer_track_end,
        auto_art: store.auto_art,
        data_dir: state.data_dir.display().to_string(),
        drag: drag.map(|d| d.kind),
        folders: store
            .library_roots
            .iter()
            .map(|r| r.path.display().to_string())
            .collect(),
        close_to_tray: state.ui.close_to_tray,
        notify_tracks: state.ui.notify_tracks,
    }
}

pub fn page(state: &State) -> ViewNode<Msg> {
    view(state)
}

fn view(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Settings);
    let view = settings_view(state);
    let geo = state.geo.clone();
    let canvas = surface(Surface::Settings, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        p.fill_visible(cx, pal.surface);
        let w = cx.size().width.0;
        let x0 = PAD;
        let cw = (w - PAD * 2.0).min(820.0);
        super::page_title(&mut p, x0, 0.0, cw, "设置");
        let mut y = 72.0;
        let hot = |region: u64| hover == Some(region);

        let section = |p: &mut P, y: &mut f32, title: &str| {
            p.text_ex(
                title,
                x0,
                *y,
                cw,
                28.0,
                17.0,
                pal.text,
                HorizontalAlign::Start,
                TextWeight::Bold,
            );
            *y += 38.0;
        };
        let card = |p: &mut P, y: f32, h: f32| p.round(x0, y, cw, h, 14.0, pal.raised);
        let label = |p: &mut P, y: f32, h: f32, title: &str, detail: &str| {
            if detail.is_empty() {
                p.text(title, x0 + 20.0, y, cw * 0.55, h, 13.5, pal.text);
            } else {
                p.text(
                    title,
                    x0 + 20.0,
                    y + h / 2.0 - 21.0,
                    cw * 0.6,
                    22.0,
                    13.5,
                    pal.text,
                );
                p.text(
                    detail,
                    x0 + 20.0,
                    y + h / 2.0 + 1.0,
                    cw * 0.62,
                    20.0,
                    12.0,
                    pal.text3,
                );
            }
        };
        let toggle = |p: &mut P, y: f32, h: f32, on: bool, region: u64| {
            let tx = x0 + cw - 64.0;
            let ty = y + h / 2.0 - 11.0;
            let fill = if on {
                if hot(region) {
                    pal.control
                } else {
                    pal.control
                }
            } else if hot(region) {
                pal.text3
            } else {
                pal.line_strong
            };
            p.round(tx, ty, 44.0, 22.0, 11.0, fill);
            let knob = if on { tx + 25.0 } else { tx + 3.0 };
            p.circle(
                knob + 8.0,
                ty + 11.0,
                8.0,
                if on { pal.on_control } else { pal.surface },
            );
            p.hover(region, tx - 4.0, ty - 4.0, 52.0, 30.0);
        };
        let chip = |p: &mut P, x: f32, y: f32, w: f32, text: &str, active: bool, region: u64| {
            let fill = if active {
                pal.control
            } else if hot(region) {
                pal.pressed
            } else {
                pal.surface
            };
            p.round(x, y, w, 30.0, 15.0, fill);
            p.text_ex(
                text,
                x,
                y,
                w,
                30.0,
                12.5,
                if active { pal.on_control } else { pal.text2 },
                HorizontalAlign::Center,
                if active {
                    TextWeight::Semibold
                } else {
                    TextWeight::Regular
                },
            );
            p.hover(region, x, y, w, 30.0);
        };
        let divider = |p: &mut P, y: f32| p.hline(x0 + 20.0, y, cw - 40.0, pal.line);
        let row_h = 64.0;

        // Library folders.
        section(&mut p, &mut y, "音乐文件夹");
        let folder_rows = view.folders.len().max(1);
        let folders_h = folder_rows as f32 * 48.0 + 64.0;
        card(&mut p, y, folders_h);
        if view.folders.is_empty() {
            p.text(
                "还没有添加文件夹",
                x0 + 20.0,
                y + 8.0,
                cw - 40.0,
                40.0,
                13.0,
                pal.text3,
            );
        }
        for (i, folder) in view.folders.iter().enumerate() {
            let fy = y + 8.0 + i as f32 * 48.0;
            p.glyph(g::FOLDER, x0 + 18.0, fy, 24.0, 40.0, 14.0, pal.text3);
            p.text(folder, x0 + 50.0, fy, cw - 170.0, 40.0, 13.0, pal.text);
            chip(
                &mut p,
                x0 + cw - 92.0,
                fy + 5.0,
                72.0,
                "移除",
                false,
                rid(R_FOLDER_REMOVE, i as u32),
            );
        }
        let by = y + folders_h - 48.0;
        chip(
            &mut p,
            x0 + 20.0,
            by,
            108.0,
            "添加文件夹",
            false,
            rid(R_FOLDER_ADD, 0),
        );
        chip(
            &mut p,
            x0 + 136.0,
            by,
            108.0,
            "重新扫描",
            false,
            rid(R_FOLDER_SCAN, 0),
        );
        y += folders_h + 28.0;

        // Window.
        section(&mut p, &mut y, "窗口");
        card(&mut p, y, row_h * 2.0);
        label(
            &mut p,
            y,
            row_h,
            "关闭窗口时最小化到托盘",
            "音乐不停，点通知区域的图标恢复",
        );
        toggle(
            &mut p,
            y,
            row_h,
            view.close_to_tray,
            rid(R_TOGGLE, T_CLOSE_TO_TRAY),
        );
        divider(&mut p, y + row_h);
        label(
            &mut p,
            y + row_h,
            row_h,
            "在托盘时通知切歌",
            "窗口隐藏后，换歌时显示一条静音通知",
        );
        toggle(
            &mut p,
            y + row_h,
            row_h,
            view.notify_tracks,
            rid(R_TOGGLE, T_NOTIFY_TRACKS),
        );
        y += row_h * 2.0 + 28.0;

        // Playback.
        section(&mut p, &mut y, "播放");
        card(&mut p, y, row_h * 4.0);
        label(&mut p, y, row_h, "在线音质", "");
        let mut cx_ = x0 + cw - 20.0;
        for (i, (value, name)) in QUALITIES.iter().enumerate().rev() {
            cx_ -= 72.0;
            chip(
                &mut p,
                cx_,
                y + 17.0,
                66.0,
                name,
                view.quality == *value,
                rid(R_QUALITY, i as u32),
            );
        }
        divider(&mut p, y + row_h);
        label(
            &mut p,
            y + row_h,
            row_h,
            "自动跳过无法播放的歌曲",
            "出错时继续播放下一首",
        );
        toggle(
            &mut p,
            y + row_h,
            row_h,
            view.auto_skip,
            rid(R_TOGGLE, T_AUTO_SKIP),
        );
        divider(&mut p, y + row_h * 2.0);
        label(
            &mut p,
            y + row_h * 2.0,
            row_h,
            "自动换源",
            "当前平台播放失败时换一个平台",
        );
        toggle(
            &mut p,
            y + row_h * 2.0,
            row_h,
            view.auto_switch,
            rid(R_TOGGLE, T_AUTO_SWITCH),
        );
        divider(&mut p, y + row_h * 3.0);
        label(&mut p, y + row_h * 3.0, row_h, "播放在线歌曲前确认地址", "");
        toggle(
            &mut p,
            y + row_h * 3.0,
            row_h,
            view.confirm_audio,
            rid(R_TOGGLE, T_CONFIRM_AUDIO),
        );
        y += row_h * 4.0 + 28.0;

        // Output.
        section(&mut p, &mut y, "音频输出");
        let device_rows = view.devices.len() + 1;
        let devices_h = 48.0 + device_rows as f32 * 40.0 + 12.0;
        card(&mut p, y, devices_h + row_h * 2.0);
        p.text(
            "输出设备",
            x0 + 20.0,
            y + 12.0,
            cw * 0.5,
            24.0,
            13.5,
            pal.text,
        );
        let refresh_hot = hot(rid(R_REFRESH_DEVICES, 0));
        p.text_r(
            "刷新",
            x0 + cw - 84.0,
            y + 12.0,
            64.0,
            24.0,
            12.5,
            if refresh_hot { pal.text } else { pal.text3 },
        );
        p.hover(
            rid(R_REFRESH_DEVICES, 0),
            x0 + cw - 64.0,
            y + 12.0,
            44.0,
            24.0,
        );
        let mut dy = y + 48.0;
        let radio = |p: &mut P, dy: f32, title: &str, detail: &str, active: bool, region: u64| {
            if hot(region) {
                p.round(x0 + 10.0, dy, cw - 20.0, 36.0, 8.0, pal.hover);
            }
            p.circle(
                x0 + 30.0,
                dy + 18.0,
                8.0,
                if active { pal.control } else { pal.line_strong },
            );
            p.circle(
                x0 + 30.0,
                dy + 18.0,
                if active { 3.5 } else { 6.5 },
                if active { pal.on_control } else { pal.raised },
            );
            p.text(title, x0 + 48.0, dy, cw - 200.0, 36.0, 13.0, pal.text);
            p.text_r(detail, x0 + cw - 180.0, dy, 160.0, 36.0, 12.0, pal.text3);
            p.hover(region, x0 + 10.0, dy, cw - 20.0, 36.0);
        };
        let follow = matches!(view.selection, OutputSelection::Default);
        radio(
            &mut p,
            dy,
            "跟随系统默认设备",
            if follow {
                view.active.as_deref().unwrap_or("")
            } else {
                ""
            },
            follow,
            rid(R_DEVICE, 0),
        );
        dy += 40.0;
        for (i, (id, name, is_default)) in view.devices.iter().enumerate() {
            let active = matches!(&view.selection, OutputSelection::Device { id: selected } if selected == id);
            radio(
                &mut p,
                dy,
                name,
                if *is_default { "系统默认" } else { "" },
                active,
                rid(R_DEVICE, i as u32 + 1),
            );
            dy += 40.0;
        }
        let ry = y + devices_h;
        divider(&mut p, ry);
        label(&mut p, ry, row_h, "静音", "");
        toggle(&mut p, ry, row_h, view.muted, rid(R_TOGGLE, T_MUTE));
        divider(&mut p, ry + row_h);
        label(&mut p, ry + row_h, row_h, "设备变化时暂停", "");
        toggle(
            &mut p,
            ry + row_h,
            row_h,
            view.pause_on_change,
            rid(R_TOGGLE, T_PAUSE_ON_CHANGE),
        );
        y += devices_h + row_h * 2.0 + 28.0;

        // Equalizer.
        section(&mut p, &mut y, "均衡器");
        let eq_h = 340.0;
        card(&mut p, y, eq_h);
        let mut px_ = x0 + 20.0;
        let mut py = y + 16.0;
        for (i, preset) in EqualizerPreset::ALL.iter().enumerate() {
            let cw_ = 60.0;
            if px_ + cw_ > x0 + cw - 90.0 {
                px_ = x0 + 20.0;
                py += 38.0;
            }
            chip(
                &mut p,
                px_,
                py,
                cw_,
                preset_name(*preset),
                view.preset == Some(*preset),
                rid(R_PRESET, i as u32),
            );
            px_ += cw_ + 8.0;
        }
        let reset_hot = hot(rid(R_EQ_RESET, 0));
        p.text_r(
            "重置",
            x0 + cw - 84.0,
            y + 16.0,
            64.0,
            30.0,
            12.5,
            if reset_hot { pal.text } else { pal.text3 },
        );
        p.hover(rid(R_EQ_RESET, 0), x0 + cw - 64.0, y + 16.0, 44.0, 30.0);
        let track_top = py + 64.0;
        let track_h = y + eq_h - 44.0 - track_top;
        let span = cw - 40.0;
        let step = span / 10.0;
        let zero_y = track_top + track_h / 2.0;
        p.hline(x0 + 20.0, zero_y, span, with_alpha(pal.text3, 0x40));
        for (i, gain) in view.eq.iter().enumerate() {
            let cx0 = x0 + 20.0 + step * i as f32 + step / 2.0;
            let region = rid(R_BAND, i as u32);
            let active = hot(region) || view.drag == Some(DragKind::Band(i));
            p.round(cx0 - 2.0, track_top, 4.0, track_h, 2.0, pal.line_strong);
            let t = ((gain - EQUALIZER_MIN_GAIN_DB)
                / (EQUALIZER_MAX_GAIN_DB - EQUALIZER_MIN_GAIN_DB)) as f32;
            let ky = track_top + track_h * (1.0 - t);
            let (a, b) = if ky < zero_y {
                (ky, zero_y)
            } else {
                (zero_y, ky)
            };
            p.round(cx0 - 2.0, a, 4.0, (b - a).max(1.0), 2.0, pal.control);
            p.circle(cx0, ky, if active { 9.0 } else { 7.0 }, pal.control);
            p.text_c(
                &format!("{:+.0}", gain),
                cx0 - 30.0,
                track_top - 26.0,
                60.0,
                20.0,
                11.5,
                if active { pal.text } else { pal.text3 },
            );
            let freq = EQUALIZER_FREQUENCIES_HZ[i];
            let f = if freq >= 1000 {
                format!("{}k", freq / 1000)
            } else {
                freq.to_string()
            };
            p.text_c(
                &f,
                cx0 - 30.0,
                track_top + track_h + 8.0,
                60.0,
                20.0,
                11.5,
                pal.text3,
            );
            geo.set(
                Surface::Settings,
                region,
                cx0 - 10.0,
                track_top,
                20.0,
                track_h,
            );
            p.hover(
                region,
                cx0 - step / 2.0 + 4.0,
                track_top - 10.0,
                step - 8.0,
                track_h + 20.0,
            );
        }
        y += eq_h + 28.0;

        // Speed and pitch.
        section(&mut p, &mut y, "播放速度与音调");
        card(&mut p, y, row_h * 3.0);
        let slider =
            |p: &mut P, y: f32, t: f64, text: &str, region: u64, reset: u64, dragging: bool| {
                let sx = x0 + cw * 0.42;
                let sw = cw * 0.58 - 140.0;
                let sy = y + row_h / 2.0;
                let active = hot(region) || dragging;
                p.round(sx, sy - 2.0, sw, 4.0, 2.0, pal.line_strong);
                p.round(sx, sy - 2.0, sw * t as f32, 4.0, 2.0, pal.control);
                p.circle(
                    sx + sw * t as f32,
                    sy,
                    if active { 9.0 } else { 7.0 },
                    pal.control,
                );
                geo.set(Surface::Settings, region, sx, sy - 10.0, sw, 20.0);
                p.hover(region, sx - 10.0, sy - 14.0, sw + 20.0, 28.0);
                let rhot = hot(reset);
                p.round(
                    x0 + cw - 104.0,
                    sy - 14.0,
                    84.0,
                    28.0,
                    14.0,
                    if rhot { pal.pressed } else { pal.surface },
                );
                p.text_c(text, x0 + cw - 104.0, sy - 14.0, 84.0, 28.0, 12.5, pal.text);
                p.hover(reset, x0 + cw - 104.0, sy - 14.0, 84.0, 28.0);
            };
        label(
            &mut p,
            y,
            row_h,
            "播放速度",
            "0.5× – 2.0×，点击数值恢复 1.0×",
        );
        slider(
            &mut p,
            y,
            rate_to_t(view.rate),
            &format!("{:.2}×", view.rate),
            rid(R_RATE, 0),
            rid(R_RATE_RESET, 0),
            view.drag == Some(DragKind::Rate),
        );
        divider(&mut p, y + row_h);
        label(
            &mut p,
            y + row_h,
            row_h,
            "变速时保持音调",
            "关闭后变速会同时改变音高",
        );
        toggle(
            &mut p,
            y + row_h,
            row_h,
            view.preserves_pitch,
            rid(R_TOGGLE, T_PRESERVE_PITCH),
        );
        divider(&mut p, y + row_h * 2.0);
        let semis = (12.0 * view.pitch.log2()).round();
        label(
            &mut p,
            y + row_h * 2.0,
            row_h,
            "移调",
            "以半音为单位升降调，不改变速度",
        );
        slider(
            &mut p,
            y + row_h * 2.0,
            pitch_to_t(view.pitch),
            &if semis == 0.0 {
                "原调".to_string()
            } else {
                format!("{semis:+.0} 半音")
            },
            rid(R_PITCH, 0),
            rid(R_PITCH_RESET, 0),
            view.drag == Some(DragKind::Pitch),
        );
        y += row_h * 3.0 + 28.0;

        // Sleep timer.
        section(&mut p, &mut y, "定时停止");
        card(&mut p, y, row_h * 2.0);
        match view.timer {
            Some((status, remaining)) => {
                let text = if status == TimeoutStopStatus::WaitingForTrackEnd {
                    "时间到，当前歌曲播完后停止".to_string()
                } else {
                    let s = remaining / 1000;
                    format!("将在 {}:{:02} 后停止播放", s / 60, s % 60)
                };
                p.glyph(g::TIMER, x0 + 16.0, y, 28.0, row_h, 15.0, pal.text2);
                p.text(&text, x0 + 48.0, y, cw - 200.0, row_h, 13.5, pal.text);
                let region = rid(R_TIMER_CANCEL, 0);
                chip(
                    &mut p,
                    x0 + cw - 104.0,
                    y + 17.0,
                    84.0,
                    "取消定时",
                    false,
                    region,
                );
            }
            None => {
                label(&mut p, y, row_h, "停止时间", "");
                let mut tx = x0 + cw - 20.0;
                for (i, minutes) in TIMERS.iter().enumerate().rev() {
                    tx -= 70.0;
                    chip(
                        &mut p,
                        tx,
                        y + 17.0,
                        64.0,
                        &format!("{minutes} 分钟"),
                        false,
                        rid(R_TIMER, i as u32),
                    );
                }
            }
        }
        divider(&mut p, y + row_h);
        label(&mut p, y + row_h, row_h, "等当前歌曲播完再停止", "");
        toggle(
            &mut p,
            y + row_h,
            row_h,
            view.timer_track_end,
            rid(R_TOGGLE, T_TIMER_TRACK_END),
        );
        y += row_h * 2.0 + 28.0;

        // Artwork and caches.
        section(&mut p, &mut y, "封面与缓存");
        card(&mut p, y, row_h * 2.0);
        label(&mut p, y, row_h, "自动加载在线封面", "");
        toggle(&mut p, y, row_h, view.auto_art, rid(R_TOGGLE, T_AUTO_ART));
        divider(&mut p, y + row_h);
        label(&mut p, y + row_h, row_h, "清理缓存", "");
        let mut bx = x0 + cw - 20.0;
        for (i, name) in ["播放地址", "图片", "音频"].iter().enumerate() {
            bx -= 92.0;
            chip(
                &mut p,
                bx,
                y + row_h + 17.0,
                86.0,
                &format!("清理{name}"),
                false,
                rid(R_CLEAR, i as u32),
            );
        }
        y += row_h * 2.0 + 28.0;

        section(&mut p, &mut y, "外部接口");
        card(&mut p, y, row_h * 2.0);
        label(
            &mut p,
            y,
            row_h,
            "LX 兼容 HTTP API",
            "供本机歌词工具及其他应用调用",
        );
        toggle(
            &mut p,
            y,
            row_h,
            view.api_enabled,
            rid(R_TOGGLE, T_OPEN_API),
        );
        divider(&mut p, y + row_h);
        let api_status = if !view.api.error.is_empty() {
            view.api.error.clone()
        } else if !view.api.address.is_empty() {
            view.api.address.clone()
        } else if view.api_enabled {
            "正在启动…".into()
        } else {
            "未启用".into()
        };
        label(
            &mut p,
            y + row_h,
            row_h,
            &api_status,
            "状态、歌词、播放控制与实时订阅；仅监听本机",
        );
        chip(
            &mut p,
            x0 + cw - 146.0,
            y + row_h + 17.0,
            126.0,
            &format!("端口 {}", view.api_port),
            false,
            rid(R_API_PORT, 0),
        );
        y += row_h * 2.0 + 28.0;

        // About.
        section(&mut p, &mut y, "关于");
        card(&mut p, y, row_h + 16.0);
        if let Some(logo) = crate::art::logo((40.0 * cx.scale_factor()).round() as u32) {
            p.image(&logo, x0 + 20.0, y + 20.0, 40.0, 40.0);
        }
        p.text_b(
            &format!("栖声 {}", env!("CARGO_PKG_VERSION")),
            x0 + 76.0,
            y + 16.0,
            cw - 100.0,
            24.0,
            14.0,
            pal.text,
        );
        p.text(
            &format!("界面基于 ZSUI · 数据目录 {}", view.data_dir),
            x0 + 76.0,
            y + 40.0,
            cw - 100.0,
            22.0,
            12.0,
            pal.text3,
        );
        y += row_h + 16.0 + 32.0;

        p.scene.set_extent_height(Dp::new(y));
        p.finish()
    });
    scroller(state, "settings", canvas)
}

pub fn click(state: &mut State, region: u64) {
    let (kind, index) = rsplit(region);
    match kind {
        R_API_PORT => {
            state.ui.modal = Some(app::Modal::ApiPort {
                input: state.ui.api_port.to_string(),
            })
        }
        R_QUALITY => {
            if let Some((value, _)) = QUALITIES.get(index as usize) {
                app::act_command(
                    state,
                    BackendCommand::SetPlaybackQuality {
                        quality: value.to_string(),
                    },
                );
                app::act_job(state, Job::RefreshQueue);
            }
        }
        R_TOGGLE => {
            let view = settings_view(state);
            match index {
                T_AUTO_SKIP => {
                    app::act_command(
                        state,
                        BackendCommand::SetAutoSkipOnError {
                            enabled: !view.auto_skip,
                        },
                    );
                    app::act_job(state, Job::RefreshQueue);
                }
                T_AUTO_SWITCH => {
                    app::act_command(
                        state,
                        BackendCommand::SetAutoSwitchSource {
                            enabled: !view.auto_switch,
                        },
                    );
                    app::act_job(state, Job::RefreshQueue);
                }
                T_CONFIRM_AUDIO => app::act_job(state, Job::AutoApproveAudio(view.confirm_audio)),
                T_MUTE => app::act_command(state, BackendCommand::SetMute { muted: !view.muted }),
                T_PAUSE_ON_CHANGE => app::act_command(
                    state,
                    BackendCommand::SetPauseOnDeviceChange {
                        enabled: !view.pause_on_change,
                    },
                ),
                T_PRESERVE_PITCH => app::act_command(
                    state,
                    BackendCommand::SetPreservesPitch {
                        enabled: !view.preserves_pitch,
                    },
                ),
                T_TIMER_TRACK_END => state.ui.timer_track_end = !state.ui.timer_track_end,
                T_AUTO_ART => app::act_set_auto_art(state, !view.auto_art),
                T_CLOSE_TO_TRAY => app::act_set_close_to_tray(state, !view.close_to_tray),
                T_OPEN_API => app::act_set_open_api(state, !state.ui.api_enabled),
                T_NOTIFY_TRACKS => app::act_set_notify_tracks(state, !view.notify_tracks),
                _ => {}
            }
        }
        R_DEVICE => {
            let selection = if index == 0 {
                Some(OutputSelection::Default)
            } else {
                state
                    .bridge
                    .store()
                    .snapshot
                    .audio_output
                    .devices
                    .get(index as usize - 1)
                    .map(|d| OutputSelection::Device { id: d.id.clone() })
            };
            if let Some(selection) = selection {
                app::act_output(state, selection);
            }
        }
        R_REFRESH_DEVICES => app::act_command(state, BackendCommand::RefreshAudioDevices),
        R_PRESET => {
            if let Some(preset) = EqualizerPreset::ALL.get(index as usize) {
                app::act_preset(state, *preset);
            }
        }
        R_EQ_RESET => app::act_command(state, BackendCommand::ResetEqualizer),
        R_RATE_RESET => app::act_command(state, BackendCommand::SetPlaybackRate { rate: 1.0 }),
        R_PITCH_RESET => app::act_command(state, BackendCommand::SetPitchRatio { ratio: 1.0 }),
        R_TIMER => {
            if let Some(minutes) = TIMERS.get(index as usize) {
                app::act_job(
                    state,
                    Job::StartTimer {
                        seconds: f64::from(*minutes) * 60.0,
                        wait_for_track_end: state.ui.timer_track_end,
                    },
                );
            }
        }
        R_TIMER_CANCEL => app::act_job(state, Job::CancelTimer),
        R_FOLDER_ADD => app::act_add_folder(state),
        R_FOLDER_SCAN => app::act_job(state, Job::RescanLibrary),
        R_FOLDER_REMOVE => {
            let root = state
                .bridge
                .store()
                .library_roots
                .get(index as usize)
                .map(|r| r.root_id.clone());
            if let Some(root_id) = root {
                app::act_job(
                    state,
                    Job::CommandNotice(
                        BackendCommand::RemoveLibraryRoot { root_id },
                        "已移除文件夹，曲库里的歌曲和歌单保持不变".into(),
                    ),
                );
                app::act_job(state, Job::RefreshLibraryRoots);
            }
        }
        R_CLEAR => {
            let (command, text) = match index {
                0 => (BackendCommand::ClearMediaCache, "已清理播放地址缓存"),
                1 => (BackendCommand::ClearImageCache, "已清理图片缓存"),
                _ => (BackendCommand::ClearAudioCache, "已清理音频缓存"),
            };
            app::act_job(state, Job::CommandNotice(command, text.into()));
        }
        _ => {}
    }
}

#[cfg(test)]
mod pointer_tests {
    use super::*;
    use lx_gpui_player::{
        backend::{BackendConfig, BackendHandle},
        platforms::{
            HttpRequest, HttpResponse, HttpTransport, ProviderContext, ProviderError,
            ProviderRegistry,
        },
    };
    use std::{
        sync::Arc,
        time::{Duration, Instant},
    };

    struct Offline;
    impl HttpTransport for Offline {
        fn execute(
            &self,
            _: HttpRequest,
            _: &ProviderContext,
        ) -> Result<HttpResponse, ProviderError> {
            Err(ProviderError::Cancelled)
        }
    }
    struct Running {
        state: State,
        backend: BackendHandle,
        _directory: tempfile::TempDir,
    }
    impl Running {
        fn new() -> Self {
            let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
            let mut config = BackendConfig::headless(directory.path().join("profile"));
            config.provider_registry = Some(Arc::new(ProviderRegistry::new(Arc::new(Offline))));
            let backend = BackendHandle::start(config).unwrap();
            let bridge =
                crate::bridge::Bridge::start(backend.clone(), zsui::UiInvalidationHandle::new());
            let mut state = State::new(bridge, directory.path().join("profile"));
            state.ui.page = app::Page::Settings;
            state.ui.api_port = 0;
            let running = Self {
                state,
                backend,
                _directory: directory,
            };
            running.wait(|s| s.bridge.store().queue.is_some());
            running
        }
        fn wait(&self, predicate: impl Fn(&State) -> bool) {
            let end = Instant::now() + Duration::from_secs(5);
            while !predicate(&self.state) {
                assert!(Instant::now() < end, "Setting did not update");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        fn pointer(&mut self, index: u32, phase: zsui::ZsCanvasPointerPhase, inside: bool) {
            let event = zsui::ZsCanvasPointerEvent::new(
                Surface::Settings.widget(),
                phase,
                zsui::ZsCanvasPoint::new(Dp::new(400.0), Dp::new(350.0)),
                zsui::ZsPointerButton::Primary,
                zsui::ZsPointerModifiers::default(),
                inside,
            )
            .with_region(Some(rid(R_TOGGLE, index)));
            app::update(
                &mut self.state,
                Msg::Ptr(Surface::Settings, event),
                &mut zsui::AppCx::new(),
            );
        }
        fn click(&mut self, index: u32) {
            self.pointer(index, zsui::ZsCanvasPointerPhase::Pressed, true);
            self.pointer(index, zsui::ZsCanvasPointerPhase::Released, true);
        }
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.state.bridge.shutdown();
            self.backend.shutdown(Duration::from_secs(15)).unwrap();
        }
    }

    #[test]
    fn settings_toggles_apply_through_pointer_messages_and_restore() {
        let mut running = Running::new();
        let cases: &[(u32, fn(&SettingsView) -> bool)] = &[
            (T_CLOSE_TO_TRAY, |v| v.close_to_tray),
            (T_NOTIFY_TRACKS, |v| v.notify_tracks),
            (T_AUTO_SKIP, |v| v.auto_skip),
            (T_AUTO_SWITCH, |v| v.auto_switch),
            (T_CONFIRM_AUDIO, |v| v.confirm_audio),
            (T_MUTE, |v| v.muted),
            (T_PAUSE_ON_CHANGE, |v| v.pause_on_change),
            (T_PRESERVE_PITCH, |v| v.preserves_pitch),
            (T_TIMER_TRACK_END, |v| v.timer_track_end),
            (T_AUTO_ART, |v| v.auto_art),
        ];
        for &(index, value) in cases {
            let before = value(&settings_view(&running.state));
            running.click(index);
            running.wait(|s| value(&settings_view(s)) != before);
            running.click(index);
            running.wait(|s| value(&settings_view(s)) == before);
        }
        running.click(T_OPEN_API);
        running.wait(|s| {
            s.ui.api_enabled
                && s.bridge.store().api.state == lx_gpui_player::open_api::ServerState::Listening
        });
        running.click(T_OPEN_API);
        running.wait(|s| {
            !s.ui.api_enabled
                && s.bridge.store().api.state == lx_gpui_player::open_api::ServerState::Disabled
        });
        let prefs =
            std::fs::read_to_string(running.state.data_dir.join("lx-native-ui.conf")).unwrap();
        assert!(prefs.contains("close_to_tray=1") && prefs.contains("api_enabled=0"));
    }

    #[test]
    fn clipped_release_or_cancel_cannot_activate_a_setting() {
        let mut running = Running::new();
        assert!(running.state.ui.close_to_tray);
        running.pointer(T_CLOSE_TO_TRAY, zsui::ZsCanvasPointerPhase::Pressed, true);
        // A partially clipped region can still have the same local region id.
        running.pointer(T_CLOSE_TO_TRAY, zsui::ZsCanvasPointerPhase::Released, false);
        assert!(running.state.ui.close_to_tray);
        running.pointer(T_CLOSE_TO_TRAY, zsui::ZsCanvasPointerPhase::Pressed, true);
        running.pointer(
            T_CLOSE_TO_TRAY,
            zsui::ZsCanvasPointerPhase::Cancelled,
            false,
        );
        running.pointer(T_CLOSE_TO_TRAY, zsui::ZsCanvasPointerPhase::Released, true);
        assert!(running.state.ui.close_to_tray);
        running.click(T_CLOSE_TO_TRAY);
        assert!(!running.state.ui.close_to_tray);
    }
}
