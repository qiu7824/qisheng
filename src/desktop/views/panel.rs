//! The player panel on the right: the record that is playing, always in
//! view. The cover sits on a quiet wash of its own color; under it the
//! title, the timeline and the transport, then lyrics or the queue, and the
//! volume at the foot. Clicking the cover opens the full-screen stage.

use super::{
    bars,
    covers::{self, Covers},
    now::{self, Cover, Now, QueueRow, R_SEEK, R_VOLUME},
    surface,
};
use crate::{
    app::{self, DragKind, MenuKind, Msg, PanelTab, State, Surface},
    bridge::Job,
    fmt::{clock, phase_ms},
    paint::{P, measure, measure_num, rid, rsplit},
    theme::{Palette, glyph as g, mix, tint, with_alpha},
};
use lx_gpui_player::{backend::BackendCommand, playback::queue::PlayMode};
use std::sync::Arc;
use zsui::{
    AppCx, Color, Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext,
    ZsCanvasWheelEvent, ZsWindowCommand, column,
};

pub const TOP_H: f32 = 40.0;
const PAD: f32 = 24.0;
const FOOT_H: f32 = 52.0;
const CAPTION_W: f32 = 46.0;

const P_PLAY: u32 = 10;
const P_PREV: u32 = 11;
const P_NEXT: u32 = 12;
const P_MODE: u32 = 13;
const P_MORE: u32 = 14;
const P_LOVE: u32 = 15;
const P_COVER: u32 = 16;
const P_TAB: u32 = 17;
const P_STAGE: u32 = 18;
const P_MUTE: u32 = 19;
const P_ARTIST: u32 = 21;
const P_APPROVE: u32 = 22;
const P_TIMER: u32 = 23;
const P_LINE: u32 = 30;
const P_QROW: u32 = 31;
const P_QREMOVE: u32 = 32;
const P_QCLEAR: u32 = 33;
const P_BODY: u32 = 34;
const C_MIN: u32 = 40;

const Q_ROW_H: f32 = 52.0;
const LINE_H: f32 = 34.0;
const SUB_H: f32 = 18.0;
const ACTIVE: f32 = 16.0;
const IDLE: f32 = 14.0;

/// Where the panel's parts sit for a canvas of height `h`.
struct Layout {
    cover: f32,
    cover_x: f32,
    cover_y: f32,
    title_y: f32,
    progress_y: f32,
    transport_y: f32,
    tabs_y: f32,
    body_y: f32,
    foot_y: f32,
}

fn layout(w: f32, h: f32) -> Layout {
    let cover = (w - PAD * 2.0).min(h - 420.0).max(140.0);
    let cover_y = 4.0;
    let title_y = cover_y + cover + 20.0;
    let progress_y = title_y + 62.0;
    let transport_y = progress_y + 30.0;
    let tabs_y = transport_y + 66.0;
    Layout {
        cover,
        cover_x: (w - cover) / 2.0,
        cover_y,
        title_y,
        progress_y,
        transport_y,
        tabs_y,
        body_y: tabs_y + 40.0,
        foot_y: h - FOOT_H,
    }
}

/// The wash behind the top of the panel, from the playing cover.
fn wash(pal: Palette, now: &Now) -> Option<Color> {
    now.cover.art().map(|art| tint(art.tone, pal.dark))
}

pub fn view(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let now = now::now(state);
    let wash = wash(pal, &now);
    super::backed(
        pal.panel,
        column([top(state, wash), body(state, now, wash).flex(1.0)]),
    )
}

fn top(state: &State, wash: Option<Color>) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::PanelTop);
    let maximized = state.ui.maximized;
    surface(Surface::PanelTop, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let w = cx.size().width.0;
        p.fill(0.0, 0.0, w, TOP_H, pal.panel);
        if let Some(wash) = wash {
            p.fill(0.0, 0.0, w, TOP_H, with_alpha(wash, wash_alpha(pal)));
        }
        p.vline(0.0, 0.0, TOP_H, pal.line);
        captions(&mut p, w, maximized, hover, C_MIN);
        p.finish()
    })
    .window_drag_region()
    .height(Dp::new(TOP_H))
}

fn wash_alpha(pal: Palette) -> u8 {
    if pal.dark { 0x99 } else { 0xb4 }
}

/// The caption buttons; regions are `base`, `base + 1` and `base + 2`.
pub fn captions(p: &mut P, w: f32, maximized: bool, hover: Option<u64>, base: u32) {
    let pal = p.pal;
    let restore = if maximized { g::RESTORE } else { g::MAX };
    for (i, glyph) in [g::MIN, restore, g::CLOSE].into_iter().enumerate() {
        let x = w - CAPTION_W * (3 - i) as f32;
        let region = rid(base + i as u32, 0);
        let hot = hover == Some(region);
        let color = if hot && i == 2 {
            p.fill(x, 0.0, CAPTION_W, 32.0, pal.close_hover);
            pal.white
        } else {
            if hot {
                p.fill(x, 0.0, CAPTION_W, 32.0, with_alpha(pal.text, 0x14));
            }
            pal.text2
        };
        p.glyph(glyph, x, 0.0, CAPTION_W, 32.0, 9.5, color);
        p.hover_plain(region, x, 0.0, CAPTION_W, 32.0);
        let label = match i {
            0 => "最小化",
            1 if maximized => "向下还原",
            1 => "最大化",
            _ => "关闭",
        };
        p.tip(region, label);
    }
}

/// Runs a caption button; `index` 0–2 is minimize, maximize, close.
pub fn caption_click(index: u32, cx: &mut AppCx) {
    cx.window_command(match index {
        0 => ZsWindowCommand::Minimize,
        1 => ZsWindowCommand::ToggleMaximize,
        _ => ZsWindowCommand::Close,
    });
}

fn body(state: &State, now: Now, wash: Option<Color>) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Panel);
    let dragging = state.ui.drag.map(|d| d.kind);
    let tab = state.ui.panel_tab;
    let geo = state.geo.clone();
    let covers = Covers::new(state);
    let (queue, current_entry) = now::queue_rows(state);
    let scroll = state.ui.panel_scroll;
    let (origin, offset_ms, timer) = {
        let store = state.bridge.store();
        (
            now.origin.clone(),
            store.lyric_offset_ms,
            store.timer.as_ref().and_then(|(t, at)| {
                use lx_gpui_player::playback::timeout_stop::TimeoutStopStatus as T;
                matches!(t.status, T::Armed | T::WaitingForTrackEnd).then(|| {
                    t.remaining_millis
                        .map(|ms| (ms as f64 / 1000.0 - at.elapsed().as_secs_f64()).max(0.0))
                        .unwrap_or(0.0)
                })
            }),
        )
    };
    let phase = phase_ms();
    surface(Surface::Panel, move |cx: &ZsCanvasLayoutContext<'_>| {
        let _cover_frame = covers.begin_frame(cx);
        let mut p = P::new(pal);
        let size = cx.size();
        let (w, h) = (size.width.0, size.height.0);
        let scale = cx.scale_factor();
        let l = layout(w, h);
        let hot = |kind: u32| hover == Some(rid(kind, 0));
        p.fill(0.0, 0.0, w, h, pal.panel);
        if let Some(wash) = wash {
            p.vgradient(
                0.0,
                0.0,
                w,
                l.cover_y + l.cover * 0.95,
                with_alpha(wash, wash_alpha(pal)),
                with_alpha(wash, 0),
                0.0,
            );
        }
        p.vline(0.0, 0.0, h, pal.line);

        // The cover.
        let (cx0, cy0, cs) = (l.cover_x, l.cover_y, l.cover);
        if now.has_track {
            p.shadow_ex(
                cx0,
                cy0,
                cs,
                cs,
                10.0,
                if pal.dark { 0x90 } else { 0x48 },
                10.0,
                22.0,
            );
        }
        match (&now.cover, now.has_track) {
            (Cover::Art(art), _) => covers::image_at(&mut p, art, cx0, cy0, cs, 10.0, scale),
            (_, true) => covers::drawn(&mut p, cx0, cy0, cs, 10.0, &now.album_or_title(), None),
            (_, false) => {
                p.round(cx0, cy0, cs, cs, 10.0, pal.raised);
                p.glyph(g::DISC, cx0, cy0, cs, cs, cs * 0.18, pal.text3);
            }
        }
        if now.has_track {
            let region = rid(P_COVER, 0);
            if hot(P_COVER) {
                p.round(cx0, cy0, cs, cs, 10.0, Color::rgba(0, 0, 0, 0x30));
                p.circle(
                    cx0 + cs - 26.0,
                    cy0 + cs - 26.0,
                    16.0,
                    Color::rgba(0, 0, 0, 0x88),
                );
                p.glyph(
                    g::EXPAND,
                    cx0 + cs - 42.0,
                    cy0 + cs - 42.0,
                    32.0,
                    32.0,
                    11.0,
                    pal.white,
                );
            }
            p.hover(region, cx0, cy0, cs, cs);
            p.tip(region, "全屏播放");
            if matches!(now.cover, Cover::Pending) {
                let label = "显示封面";
                let bw = measure(cx, label, 12.0, TextWeight::Semibold) + 24.0;
                let (bx, by) = (cx0 + (cs - bw) / 2.0, cy0 + cs - 44.0);
                let id = rid(P_APPROVE, 0);
                p.round(
                    bx,
                    by,
                    bw,
                    28.0,
                    14.0,
                    if hot(P_APPROVE) {
                        pal.control
                    } else {
                        with_alpha(pal.control, 0xd8)
                    },
                );
                p.text_ex(
                    label,
                    bx,
                    by,
                    bw,
                    28.0,
                    12.0,
                    pal.on_control,
                    HorizontalAlign::Center,
                    TextWeight::Semibold,
                );
                p.hover(id, bx, by, bw, 28.0);
            }
        }

        // Title and artist.
        let ty = l.title_y;
        let tw = w - PAD * 2.0 - 40.0;
        if now.has_track {
            p.heading(&now.title, PAD, ty, tw, 26.0, 17.0, pal.text);
            let artist = if now.artist.is_empty() {
                "未知歌手".to_string()
            } else {
                now.artist.clone()
            };
            let line = match now.album_shown() {
                Some(album) => format!("{artist} · {album}"),
                None => artist.clone(),
            };
            let artist_hot = hot(P_ARTIST);
            p.text(
                &line,
                PAD,
                ty + 26.0,
                tw,
                20.0,
                12.5,
                if artist_hot { pal.text } else { pal.text2 },
            );
            if !now.artist.is_empty() {
                let aw = measure(cx, &artist, 12.5, TextWeight::Regular).min(tw);
                p.hover(rid(P_ARTIST, 0), PAD, ty + 26.0, aw, 20.0);
                p.tip(rid(P_ARTIST, 0), "搜索这位歌手");
            }
            let love = rid(P_LOVE, 0);
            p.icon_button(
                love,
                if now.loved { g::HEART_FILL } else { g::HEART },
                w - PAD - 14.0,
                ty + 20.0,
                34.0,
                14.0,
                hot(P_LOVE).then_some(pal.hover),
                if now.loved {
                    pal.love
                } else if hot(P_LOVE) {
                    pal.text
                } else {
                    pal.text2
                },
            );
            p.tip(love, if now.loved { "取消喜欢" } else { "喜欢" });
        } else {
            p.heading("还没有在播放", PAD, ty, tw + 40.0, 26.0, 17.0, pal.text2);
            p.text(
                "从发现、排行榜或本地音乐里挑一首歌",
                PAD,
                ty + 26.0,
                tw + 40.0,
                20.0,
                12.5,
                pal.text3,
            );
        }

        // Timeline.
        let py = l.progress_y;
        let seek_hot = hot(R_SEEK) || dragging == Some(DragKind::Seek);
        let track_w = w - PAD * 2.0;
        let bar_h = if seek_hot { 5.0 } else { 3.0 };
        p.round(
            PAD,
            py - bar_h / 2.0,
            track_w,
            bar_h,
            bar_h / 2.0,
            with_alpha(pal.text, 0x24),
        );
        if now.has_track {
            let fw = track_w * now.fraction();
            p.round(PAD, py - bar_h / 2.0, fw, bar_h, bar_h / 2.0, pal.text);
            if seek_hot {
                p.circle(PAD + fw, py, 6.5, pal.text);
            }
        }
        geo.set(Surface::Panel, rid(R_SEEK, 0), PAD, py - 8.0, track_w, 16.0);
        if now.has_track {
            p.hover(rid(R_SEEK, 0), PAD - 6.0, py - 10.0, track_w + 12.0, 20.0);
            p.name(rid(R_SEEK, 0), "播放进度");
        }
        let times_y = py + 8.0;
        p.num(
            &clock(now.position),
            PAD,
            times_y,
            80.0,
            18.0,
            11.5,
            pal.text3,
            HorizontalAlign::Start,
            TextWeight::Regular,
        );
        p.num(
            &clock(now.duration),
            w - PAD - 80.0,
            times_y,
            80.0,
            18.0,
            11.5,
            pal.text3,
            HorizontalAlign::End,
            TextWeight::Regular,
        );
        if (now.rate - 1.0).abs() > 0.001 {
            p.num(
                &format!("{:.2}×", now.rate),
                w / 2.0 - 40.0,
                times_y,
                80.0,
                18.0,
                11.5,
                pal.text2,
                HorizontalAlign::Center,
                TextWeight::Semibold,
            );
        }

        // Transport.
        let mid = l.transport_y + 28.0;
        let center = w / 2.0;
        let quiet = |kind: u32, enabled: bool| {
            if !enabled {
                pal.line_strong
            } else if hot(kind) {
                pal.text
            } else {
                pal.text2
            }
        };
        let side =
            |p: &mut P, kind: u32, x: f32, glyph: char, size: f32, color: Color, label: &str| {
                let id = rid(kind, 0);
                p.icon_button(
                    id,
                    glyph,
                    x,
                    mid,
                    40.0,
                    size,
                    hot(kind).then_some(pal.hover),
                    color,
                );
                p.tip(id, label);
            };
        let mode_on = now.mode != PlayMode::ListLoop;
        side(
            &mut p,
            P_MODE,
            center - 116.0,
            now::mode_glyph(now.mode),
            13.0,
            if mode_on {
                pal.text
            } else {
                quiet(P_MODE, true)
            },
            now::mode_name(now.mode),
        );
        if mode_on {
            p.circle(center - 116.0, mid + 15.0, 2.0, pal.text);
        }
        side(
            &mut p,
            P_PREV,
            center - 62.0,
            g::PREVIOUS,
            15.0,
            quiet(P_PREV, now.has_track),
            "上一首",
        );
        side(
            &mut p,
            P_NEXT,
            center + 62.0,
            g::NEXT,
            15.0,
            quiet(P_NEXT, now.has_track),
            "下一首",
        );
        side(
            &mut p,
            P_MORE,
            center + 116.0,
            g::MORE,
            14.0,
            quiet(P_MORE, true),
            "更多",
        );
        let active = now::is_active(now.status);
        let r = if hot(P_PLAY) { 29.0 } else { 28.0 };
        p.circle(center, mid, r, pal.control);
        p.glyph(
            if active { g::PAUSE } else { g::PLAY },
            center - 28.0 + if active { 0.0 } else { 1.5 },
            mid - 28.0,
            56.0,
            56.0,
            16.0,
            pal.on_control,
        );
        if now.loading() {
            p.spinner(
                center - 33.0,
                mid - 33.0,
                66.0,
                phase,
                pal.text2,
                with_alpha(pal.text2, 0x18),
            );
        }
        let play = rid(P_PLAY, 0);
        p.hover(play, center - 28.0, mid - 28.0, 56.0, 56.0);
        p.tip(play, if active { "暂停" } else { "播放" });

        // Tabs.
        let tabs_y = l.tabs_y;
        let mut tx = PAD;
        for (i, label) in ["歌词", "接下来"].into_iter().enumerate() {
            let id = rid(P_TAB, i as u32);
            let on = (i == 0) == (tab == PanelTab::Lyrics);
            let lw = measure(cx, label, 13.5, TextWeight::Bold);
            let color = if on || hover == Some(id) {
                pal.text
            } else {
                pal.text3
            };
            p.text_ex(
                label,
                tx,
                tabs_y,
                lw + 4.0,
                28.0,
                13.5,
                color,
                HorizontalAlign::Start,
                TextWeight::Bold,
            );
            if on {
                p.round(tx, tabs_y + 29.0, lw, 2.5, 1.25, pal.text);
            }
            p.hover(id, tx - 6.0, tabs_y, lw + 12.0, 32.0);
            tx += lw + 22.0;
        }
        if tab == PanelTab::Queue && !queue.is_empty() {
            let count = format!("{}", queue.len());
            p.num(
                &count,
                tx - 16.0,
                tabs_y,
                40.0,
                28.0,
                11.5,
                pal.text3,
                HorizontalAlign::Start,
                TextWeight::Regular,
            );
        }
        if now.has_track {
            p.tool(
                rid(P_STAGE, 0),
                g::EXPAND,
                w - PAD - 12.0,
                tabs_y + 14.0,
                30.0,
                10.5,
                hover,
                false,
                "全屏播放",
            );
        }
        p.hline(PAD, tabs_y + 32.0, w - PAD * 2.0, pal.line);

        // Lyrics or the queue.
        let body_h = (l.foot_y - l.body_y).max(0.0);
        p.push_clip(0.0, l.body_y, w, body_h);
        match tab {
            PanelTab::Lyrics => lyrics(&mut p, cx, &now, w, l.body_y, body_h, hover, offset_ms),
            PanelTab::Queue => {
                let max = queue_view(
                    &mut p,
                    cx,
                    &queue,
                    current_entry.as_deref(),
                    &origin,
                    &covers,
                    w,
                    l.body_y,
                    body_h,
                    scroll,
                    hover,
                    now::is_active(now.status),
                    phase,
                );
                geo.set(Surface::Panel, rid(P_BODY, 0), 0.0, max, 0.0, 0.0);
            }
        }
        p.pop_clip();
        // Fade the body into the panel at both ends.
        let fade = 24.0;
        p.vgradient(
            0.0,
            l.body_y,
            w,
            fade,
            pal.panel,
            with_alpha(pal.panel, 0),
            0.0,
        );
        p.vgradient(
            0.0,
            l.foot_y - fade,
            w,
            fade,
            with_alpha(pal.panel, 0),
            pal.panel,
            0.0,
        );
        p.hover_plain(rid(P_BODY, 0), 0.0, l.body_y, w, body_h);

        // Foot: volume and the sleep timer.
        let fy = l.foot_y + FOOT_H / 2.0;
        p.hline(PAD, l.foot_y, w - PAD * 2.0, pal.line);
        let mute = rid(P_MUTE, 0);
        p.icon_button(
            mute,
            now.volume_glyph(),
            PAD + 10.0,
            fy,
            30.0,
            12.5,
            hot(P_MUTE).then_some(pal.hover),
            if hot(P_MUTE) { pal.text } else { pal.text2 },
        );
        p.tip(mute, if now.muted { "取消静音" } else { "静音" });
        let vx = PAD + 34.0;
        let vw = if timer.is_some() {
            w - vx - PAD - 86.0
        } else {
            w - vx - PAD - 40.0
        };
        let shown = if now.muted {
            0.0
        } else {
            now.volume.clamp(0.0, 1.0)
        };
        let vol_hot = hot(R_VOLUME) || dragging == Some(DragKind::Volume);
        p.round(vx, fy - 1.5, vw, 3.0, 1.5, with_alpha(pal.text, 0x24));
        p.round(
            vx,
            fy - 1.5,
            vw * shown,
            3.0,
            1.5,
            if vol_hot { pal.text } else { pal.text2 },
        );
        if vol_hot {
            p.circle(vx + vw * shown, fy, 6.0, pal.text);
        }
        geo.set(Surface::Panel, rid(R_VOLUME, 0), vx, fy - 8.0, vw, 16.0);
        p.hover(rid(R_VOLUME, 0), vx - 6.0, fy - 10.0, vw + 12.0, 20.0);
        p.tip(
            rid(R_VOLUME, 0),
            &format!("音量 {}%", (shown * 100.0).round() as u32),
        );
        p.num(
            &format!("{}", (shown * 100.0).round() as u32),
            vx + vw + 8.0,
            fy - 10.0,
            28.0,
            20.0,
            11.5,
            pal.text3,
            HorizontalAlign::Start,
            TextWeight::Regular,
        );
        if let Some(left) = timer {
            let id = rid(P_TIMER, 0);
            let label = clock(left);
            let tw = measure_num(cx, &label, 11.5, TextWeight::Regular) + 30.0;
            let x = w - PAD - tw;
            p.round(
                x,
                fy - 13.0,
                tw,
                26.0,
                13.0,
                if hot(P_TIMER) {
                    pal.pressed
                } else {
                    pal.raised
                },
            );
            p.glyph(g::TIMER, x + 4.0, fy - 13.0, 22.0, 26.0, 10.5, pal.text2);
            p.num(
                &label,
                x + 22.0,
                fy - 13.0,
                tw - 24.0,
                26.0,
                11.5,
                pal.text,
                HorizontalAlign::Start,
                TextWeight::Regular,
            );
            p.hover(id, x, fy - 13.0, tw, 26.0);
            p.tip(id, "定时停止");
        }
        p.finish()
    })
    .on_canvas_wheel_with(|event| {
        let kind = event.region.map(|region| rsplit(region).0);
        matches!(
            kind,
            Some(P_BODY | P_QROW | P_QREMOVE | P_LINE | R_VOLUME | P_MUTE)
        )
        .then_some(Msg::Wheel(Surface::Panel, event))
    })
}

/// Lyrics centered in the body, the current line lit word by word.
#[allow(clippy::too_many_arguments)]
fn lyrics(
    p: &mut P,
    cx: &ZsCanvasLayoutContext<'_>,
    now: &Now,
    w: f32,
    top: f32,
    h: f32,
    hover: Option<u64>,
    offset_ms: i64,
) {
    let pal = p.pal;
    let lines = now
        .lyrics
        .as_ref()
        .map(|l| l.document.lines.as_slice())
        .unwrap_or(&[]);
    let lw = w - PAD * 2.0;
    if lines.is_empty() {
        let text = if now.has_track {
            now.lyric_placeholder()
        } else {
            "播放后在这里显示歌词"
        };
        p.text_c(text, PAD, top + h * 0.38 - 12.0, lw, 24.0, 13.0, pal.text3);
        return;
    }
    let current = now.frame.as_ref().and_then(|f| f.line_index);
    let mut tops = Vec::with_capacity(lines.len());
    let mut y = 0.0;
    for line in lines {
        tops.push(y);
        y += LINE_H + SUB_H * line.translation.len().min(1) as f32;
    }
    let shift = now::lyric_shift(1, current, &tops);
    let origin = top + h * 0.36 - shift - LINE_H / 2.0;
    for (i, line) in lines.iter().enumerate() {
        let ly = origin + tops[i];
        let block = LINE_H + SUB_H * line.translation.len().min(1) as f32;
        if ly + block < top - 8.0 || ly > top + h + 8.0 {
            continue;
        }
        let active = current == Some(i);
        let text = if line.text.trim().is_empty() {
            "♪"
        } else {
            line.text.as_str()
        };
        let region = rid(P_LINE, i as u32);
        if hover == Some(region) && !active {
            p.round(PAD - 10.0, ly + 2.0, lw + 20.0, block - 4.0, 8.0, pal.hover);
        }
        p.hover(region, PAD - 10.0, ly + 2.0, lw + 20.0, block - 4.0);
        if active {
            let tw = measure(cx, text, ACTIVE, TextWeight::Bold).min(lw);
            let tx = PAD + (lw - tw) / 2.0;
            p.text_ex(
                text,
                PAD,
                ly,
                lw,
                LINE_H,
                ACTIVE,
                with_alpha(pal.text, 0x66),
                HorizontalAlign::Center,
                TextWeight::Bold,
            );
            let sung = now::sung_width(cx, line, now.frame.as_deref(), ACTIVE, tw);
            if sung > 0.0 {
                p.push_clip(tx - 2.0, ly, sung + 2.0, LINE_H);
                p.text_ex(
                    text,
                    PAD,
                    ly,
                    lw,
                    LINE_H,
                    ACTIVE,
                    pal.text,
                    HorizontalAlign::Center,
                    TextWeight::Bold,
                );
                p.pop_clip();
            }
        } else {
            let distance = current.map(|c| c.abs_diff(i)).unwrap_or(2) as f32;
            let idle = mix(pal.text3, pal.panel, (distance * 0.12).clamp(0.0, 0.6));
            p.text_ex(
                text,
                PAD,
                ly,
                lw,
                LINE_H,
                IDLE,
                idle,
                HorizontalAlign::Center,
                TextWeight::Regular,
            );
        }
        if let Some(translation) = line.translation.first() {
            p.text_ex(
                translation,
                PAD,
                ly + LINE_H - 8.0,
                lw,
                SUB_H,
                12.0,
                if active { pal.text2 } else { pal.text3 },
                HorizontalAlign::Center,
                TextWeight::Regular,
            );
        }
    }
    let _ = offset_ms;
}

/// The queue: "play next" entries, then the list it plays from. Returns the
/// largest scroll offset.
#[allow(clippy::too_many_arguments)]
fn queue_view(
    p: &mut P,
    cx: &ZsCanvasLayoutContext<'_>,
    rows: &Arc<Vec<QueueRow>>,
    current: Option<&str>,
    origin: &str,
    covers: &Covers,
    w: f32,
    top: f32,
    h: f32,
    scroll: f32,
    hover: Option<u64>,
    playing: bool,
    phase: u64,
) -> f32 {
    let pal = p.pal;
    let scale = cx.scale_factor();
    if rows.is_empty() {
        p.text_c(
            "队列是空的",
            PAD,
            top + h * 0.36 - 12.0,
            w - PAD * 2.0,
            24.0,
            13.0,
            pal.text3,
        );
        return 0.0;
    }
    let later = rows.iter().filter(|r| r.temporary).count();
    let head_h = 34.0;
    let content = head_h * if later > 0 { 2.0 } else { 1.0 } + rows.len() as f32 * Q_ROW_H + 24.0;
    let max = (content - h).max(0.0);
    let scroll = scroll.clamp(0.0, max);
    let mut y = top + 8.0 - scroll;
    let header = |p: &mut P, y: f32, text: &str, clear: bool| {
        p.text_ex(
            text,
            PAD,
            y,
            w - PAD * 2.0 - 60.0,
            26.0,
            12.0,
            pal.text3,
            HorizontalAlign::Start,
            TextWeight::Semibold,
        );
        if clear {
            let id = rid(P_QCLEAR, 0);
            let color = if hover == Some(id) {
                pal.text
            } else {
                pal.text3
            };
            p.text_r("清空", w - PAD - 60.0, y, 60.0, 26.0, 12.0, color);
            p.hover(id, w - PAD - 44.0, y, 48.0, 26.0);
        }
    };
    for (i, row) in rows.iter().enumerate() {
        if i == 0 && later > 0 {
            header(p, y, "稍后播放", true);
            y += head_h;
        }
        if i == later {
            let from = if origin.is_empty() {
                "列表".to_string()
            } else {
                format!("来自 {origin}")
            };
            header(p, y, &from, false);
            y += head_h;
        }
        if y + Q_ROW_H >= top - 4.0 && y <= top + h + 4.0 {
            let region = rid(P_QROW, i as u32);
            let is_current = current == Some(row.entry_id.as_str());
            let hot = hover.is_some_and(|h| {
                let (k, idx) = rsplit(h);
                idx == i as u32 && (k == P_QROW || k == P_QREMOVE)
            });
            if is_current {
                p.round(
                    PAD - 8.0,
                    y + 2.0,
                    w - PAD * 2.0 + 16.0,
                    Q_ROW_H - 4.0,
                    8.0,
                    with_alpha(pal.text, 0x10),
                );
            } else if hot {
                p.round(
                    PAD - 8.0,
                    y + 2.0,
                    w - PAD * 2.0 + 16.0,
                    Q_ROW_H - 4.0,
                    8.0,
                    pal.hover,
                );
            }
            p.hover_plain(region, PAD - 8.0, y, w - PAD * 2.0 + 16.0, Q_ROW_H);
            let art = if row.list_id == "catalog" {
                covers.track(&row.track_id)
            } else {
                covers
                    .get(&row.track_id, &row.list_id, false)
                    .or_else(|| covers.url(row.image.as_deref()))
            };
            covers::draw(
                p,
                art.as_deref(),
                PAD,
                y + 8.0,
                36.0,
                5.0,
                scale,
                &row.title,
            );
            if is_current {
                p.round(PAD, y + 8.0, 36.0, 36.0, 5.0, Color::rgba(0, 0, 0, 0x66));
                bars(p, PAD + 11.0, y + 26.0, playing, phase, pal.white);
            }
            let tx = PAD + 48.0;
            let tw = w - tx - PAD - 44.0;
            p.text_ex(
                &row.title,
                tx,
                y + 8.0,
                tw,
                20.0,
                13.0,
                pal.text,
                HorizontalAlign::Start,
                if is_current {
                    TextWeight::Bold
                } else {
                    TextWeight::Regular
                },
            );
            p.text(&row.artist, tx, y + 27.0, tw, 18.0, 11.5, pal.text3);
            if hot && row.temporary {
                let remove = rid(P_QREMOVE, i as u32);
                p.tool(
                    remove,
                    g::CANCEL,
                    w - PAD - 12.0,
                    y + Q_ROW_H / 2.0,
                    26.0,
                    9.0,
                    hover,
                    false,
                    "从队列移除",
                );
            } else {
                p.num(
                    &clock(row.seconds),
                    w - PAD - 48.0,
                    y,
                    48.0,
                    Q_ROW_H,
                    11.5,
                    pal.text3,
                    HorizontalAlign::End,
                    TextWeight::Regular,
                );
            }
        }
        y += Q_ROW_H;
    }
    max
}

pub fn wheel(state: &mut State, event: ZsCanvasWheelEvent) {
    let kind = event.region.map(|region| rsplit(region).0);
    match kind {
        Some(R_VOLUME | P_MUTE) => {
            let step = if event.delta.0 > 0.0 { -0.05 } else { 0.05 };
            app::act_set_volume(state, app::act_volume(state) + step);
        }
        Some(P_BODY | P_QROW | P_QREMOVE | P_LINE) if state.ui.panel_tab == PanelTab::Queue => {
            let max = state
                .geo
                .get(Surface::Panel, rid(P_BODY, 0))
                .map(|g| g[1])
                .unwrap_or(f32::MAX);
            state.ui.panel_scroll =
                (state.ui.panel_scroll + event.delta.0).clamp(0.0, max.max(0.0));
        }
        _ => {}
    }
}

pub fn click(
    state: &mut State,
    surface: Surface,
    region: u64,
    double: bool,
    x: f32,
    y: f32,
    cx: &mut AppCx,
) {
    let (kind, index) = rsplit(region);
    if surface == Surface::PanelTop {
        if (C_MIN..C_MIN + 3).contains(&kind) {
            caption_click(kind - C_MIN, cx);
        }
        return;
    }
    match kind {
        P_PLAY => app::act_toggle_play(state),
        P_PREV => app::act_command(state, BackendCommand::Previous),
        P_NEXT => app::act_command(state, BackendCommand::Next),
        P_MODE => app::act_cycle_mode(state),
        P_MORE => app::act_open_menu(state, MenuKind::NowPlaying, Surface::Panel, x, y + 18.0),
        P_LOVE => app::act_toggle_current_love(state),
        P_COVER | P_STAGE => state.ui.stage = true,
        P_TAB => {
            state.ui.panel_tab = if index == 1 {
                app::act_job(state, Job::RefreshQueue);
                PanelTab::Queue
            } else {
                PanelTab::Lyrics
            };
        }
        P_MUTE => {
            let muted = state.bridge.store().snapshot.audio_output.preferences.muted;
            app::act_command(state, BackendCommand::SetMute { muted: !muted });
        }
        P_ARTIST => {
            let artist = state
                .bridge
                .store()
                .snapshot
                .now_playing
                .as_ref()
                .map(|t| t.artist.clone())
                .unwrap_or_default();
            if !artist.is_empty() {
                app::act_search(state, artist);
            }
        }
        P_APPROVE => {
            let key = state
                .bridge
                .store()
                .snapshot
                .now_playing
                .as_ref()
                .map(|t| t.id.clone());
            if let Some(key) = key {
                app::act_job(state, Job::ApproveArt(key));
            }
        }
        P_TIMER => app::act_open_menu(state, MenuKind::Timer, Surface::Panel, x, y - 8.0),
        P_LINE => now::seek_to_line(state, index as usize),
        P_QCLEAR => {
            app::act_command(state, BackendCommand::ClearTemporaryQueue);
            app::act_job(state, Job::RefreshQueue);
        }
        P_QROW | P_QREMOVE => {
            let (rows, _) = now::queue_rows(state);
            let Some(row) = rows.get(index as usize).cloned() else {
                return;
            };
            if kind == P_QREMOVE {
                app::act_command(
                    state,
                    BackendCommand::RemoveQueueEntry {
                        entry_id: row.entry_id,
                    },
                );
                app::act_job(state, Job::RefreshQueue);
            } else if double {
                now::play_queue_row(state, &row);
            }
        }
        _ => {}
    }
}
