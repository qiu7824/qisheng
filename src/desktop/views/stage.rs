//! The full-screen stage: the record alone in the window. A blurred, quiet
//! copy of the cover fills the background; the cover and its credits sit on
//! the left, the lyrics run large on the right, and the timeline and the
//! transport cross the bottom. Escape or the chevron goes back.

use super::{
    covers,
    now::{self, Cover, Now, R_SEEK, R_VOLUME},
    panel::{caption_click, captions},
    surface,
};
use crate::{
    app::{self, DragKind, MenuKind, Msg, State, Surface},
    fmt::{clock, phase_ms},
    paint::{P, measure, rid, rsplit},
    theme::{Palette, glyph as g, mix, tint, with_alpha},
};
use lx_gpui_player::{backend::BackendCommand, playback::queue::PlayMode};
use zsui::{AppCx, Color, Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext};

// Regions 1 and 2 are reserved for the shared seek and volume drag routes.
const S_COLLAPSE: u32 = 10;
const S_PLAY: u32 = 11;
const S_PREV: u32 = 12;
const S_NEXT: u32 = 13;
const S_MODE: u32 = 14;
const S_LOVE: u32 = 15;
const S_MORE: u32 = 16;
const S_MUTE: u32 = 17;
const S_LINE: u32 = 18;
const S_ARTIST: u32 = 19;
const C_MIN: u32 = 40;

const LINE_H: f32 = 52.0;
const SUB_H: f32 = 24.0;
const ACTIVE: f32 = 26.0;
const IDLE: f32 = 19.0;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_controls_do_not_alias_media_drag_or_caption_regions() {
        let controls = [
            S_COLLAPSE, S_PLAY, S_PREV, S_NEXT, S_MODE, S_LOVE, S_MORE, S_MUTE, S_LINE, S_ARTIST,
        ];
        let mut regions =
            std::collections::HashSet::from([R_SEEK, R_VOLUME, C_MIN, C_MIN + 1, C_MIN + 2]);
        for control in controls {
            assert!(
                regions.insert(control),
                "stage region {control} is already routed"
            );
        }
    }
}

pub fn view(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let now = now::now(state);
    let hover = state.hover(Surface::Stage);
    let dragging = state.ui.drag.map(|d| d.kind);
    let maximized = state.ui.maximized;
    let geo = state.geo.clone();
    let phase = phase_ms();
    surface(Surface::Stage, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let size = cx.size();
        let (w, h) = (size.width.0, size.height.0);
        let scale = cx.scale_factor();
        let hot = |kind: u32| hover == Some(rid(kind, 0));
        backdrop(&mut p, &now, w, h);

        // Top: back to the window, and the caption buttons.
        let collapse = rid(S_COLLAPSE, 0);
        p.icon_button(
            collapse,
            g::CHEVRON_DOWN,
            40.0,
            36.0,
            36.0,
            12.0,
            Some(with_alpha(
                pal.text,
                if hot(S_COLLAPSE) { 0x24 } else { 0x12 },
            )),
            pal.text,
        );
        p.tip(collapse, "返回 (Esc)");
        captions(&mut p, w, maximized, hover, C_MIN);

        // The record.
        let bottom = 132.0;
        let area_top = 72.0;
        let area_h = h - area_top - bottom;
        let cs = (w * 0.30).min(area_h - 110.0).clamp(180.0, 460.0);
        let col_w = w * 0.46;
        let cx0 = ((col_w - cs) / 2.0).max(56.0);
        let cy0 = area_top + ((area_h - cs - 100.0) / 2.0).max(0.0);
        p.shadow_ex(
            cx0,
            cy0,
            cs,
            cs,
            14.0,
            if pal.dark { 0xb0 } else { 0x58 },
            18.0,
            36.0,
        );
        match &now.cover {
            Cover::Art(art) => covers::image_at(&mut p, art, cx0, cy0, cs, 14.0, scale),
            _ if now.has_track => {
                covers::drawn(&mut p, cx0, cy0, cs, 14.0, &now.album_or_title(), None)
            }
            _ => {
                p.round(cx0, cy0, cs, cs, 14.0, pal.raised);
                p.glyph(g::DISC, cx0, cy0, cs, cs, cs * 0.18, pal.text3);
            }
        }
        let ty = cy0 + cs + 28.0;
        p.heading(
            if now.has_track {
                &now.title
            } else {
                "还没有在播放"
            },
            cx0,
            ty,
            cs + 40.0,
            34.0,
            24.0,
            pal.text,
        );
        if now.has_track {
            let artist = if now.artist.is_empty() {
                "未知歌手"
            } else {
                now.artist.as_str()
            };
            let aw = measure(cx, artist, 15.0, TextWeight::Regular).min(cs);
            p.text(
                artist,
                cx0,
                ty + 36.0,
                aw + 4.0,
                24.0,
                15.0,
                if hot(S_ARTIST) { pal.text } else { pal.text2 },
            );
            if !now.artist.is_empty() {
                p.hover(rid(S_ARTIST, 0), cx0, ty + 36.0, aw, 24.0);
            }
            if let Some(album) = now.album_shown() {
                p.text(album, cx0, ty + 62.0, cs, 20.0, 13.0, pal.text3);
            }
        }

        // Lyrics.
        let lx = col_w + 24.0;
        let lw = (w - lx - 72.0).max(200.0);
        lyrics(&mut p, cx, &now, lx, lw, area_top, area_h, hover);

        // Timeline and transport.
        let line_y = h - 104.0;
        let x0 = 64.0;
        let track_w = w - x0 * 2.0;
        let seek_hot = hot(R_SEEK) || dragging == Some(DragKind::Seek);
        let bar_h = if seek_hot { 5.0 } else { 3.0 };
        p.round(
            x0,
            line_y - bar_h / 2.0,
            track_w,
            bar_h,
            bar_h / 2.0,
            with_alpha(pal.text, 0x2a),
        );
        let fw = track_w * now.fraction();
        p.round(x0, line_y - bar_h / 2.0, fw, bar_h, bar_h / 2.0, pal.text);
        if seek_hot {
            p.circle(x0 + fw, line_y, 7.0, pal.text);
        }
        geo.set(
            Surface::Stage,
            rid(R_SEEK, 0),
            x0,
            line_y - 8.0,
            track_w,
            16.0,
        );
        if now.has_track {
            p.hover(
                rid(R_SEEK, 0),
                x0 - 6.0,
                line_y - 12.0,
                track_w + 12.0,
                24.0,
            );
        }
        p.num(
            &clock(now.position),
            x0,
            line_y + 10.0,
            80.0,
            18.0,
            12.0,
            pal.text2,
            HorizontalAlign::Start,
            TextWeight::Regular,
        );
        p.num(
            &clock(now.duration),
            w - x0 - 80.0,
            line_y + 10.0,
            80.0,
            18.0,
            12.0,
            pal.text2,
            HorizontalAlign::End,
            TextWeight::Regular,
        );
        let mid = h - 52.0;
        let center = w / 2.0;
        let button =
            |p: &mut P, kind: u32, x: f32, glyph: char, size: f32, color: Color, label: &str| {
                let id = rid(kind, 0);
                p.icon_button(
                    id,
                    glyph,
                    x,
                    mid,
                    44.0,
                    size,
                    hot(kind).then_some(with_alpha(pal.text, 0x14)),
                    color,
                );
                p.tip(id, label);
            };
        let soft = |kind: u32| if hot(kind) { pal.text } else { pal.text2 };
        let mode_on = now.mode != PlayMode::ListLoop;
        button(
            &mut p,
            S_MODE,
            center - 136.0,
            now::mode_glyph(now.mode),
            14.0,
            if mode_on { pal.text } else { soft(S_MODE) },
            now::mode_name(now.mode),
        );
        button(
            &mut p,
            S_PREV,
            center - 72.0,
            g::PREVIOUS,
            17.0,
            soft(S_PREV),
            "上一首",
        );
        button(
            &mut p,
            S_NEXT,
            center + 72.0,
            g::NEXT,
            17.0,
            soft(S_NEXT),
            "下一首",
        );
        button(
            &mut p,
            S_LOVE,
            center + 136.0,
            if now.loved { g::HEART_FILL } else { g::HEART },
            15.0,
            if now.loved { pal.love } else { soft(S_LOVE) },
            if now.loved { "取消喜欢" } else { "喜欢" },
        );
        let active = now::is_active(now.status);
        p.circle(
            center,
            mid,
            if hot(S_PLAY) { 31.0 } else { 30.0 },
            pal.control,
        );
        p.glyph(
            if active { g::PAUSE } else { g::PLAY },
            center - 30.0 + if active { 0.0 } else { 1.5 },
            mid - 30.0,
            60.0,
            60.0,
            17.0,
            pal.on_control,
        );
        if now.loading() {
            p.spinner(
                center - 35.0,
                mid - 35.0,
                70.0,
                phase,
                pal.text2,
                with_alpha(pal.text2, 0x18),
            );
        }
        p.hover(rid(S_PLAY, 0), center - 30.0, mid - 30.0, 60.0, 60.0);
        p.tip(rid(S_PLAY, 0), if active { "暂停" } else { "播放" });
        button(
            &mut p,
            S_MORE,
            x0 + 22.0,
            g::MORE,
            15.0,
            soft(S_MORE),
            "更多",
        );

        // Volume.
        let vw = 120.0;
        let vx = w - x0 - vw;
        let shown = if now.muted {
            0.0
        } else {
            now.volume.clamp(0.0, 1.0)
        };
        let vol_hot = hot(R_VOLUME) || dragging == Some(DragKind::Volume);
        p.round(vx, mid - 1.5, vw, 3.0, 1.5, with_alpha(pal.text, 0x2a));
        p.round(
            vx,
            mid - 1.5,
            vw * shown,
            3.0,
            1.5,
            if vol_hot { pal.text } else { pal.text2 },
        );
        if vol_hot {
            p.circle(vx + vw * shown, mid, 6.0, pal.text);
        }
        geo.set(Surface::Stage, rid(R_VOLUME, 0), vx, mid - 8.0, vw, 16.0);
        p.hover(rid(R_VOLUME, 0), vx - 6.0, mid - 12.0, vw + 12.0, 24.0);
        p.tip(
            rid(R_VOLUME, 0),
            &format!("音量 {}%", (shown * 100.0).round() as u32),
        );
        button(
            &mut p,
            S_MUTE,
            vx - 26.0,
            now.volume_glyph(),
            13.0,
            soft(S_MUTE),
            if now.muted { "取消静音" } else { "静音" },
        );
        p.finish()
    })
    .window_drag_region()
    .on_canvas_wheel_with(|event| {
        let kind = event.region.map(|region| rsplit(region).0);
        matches!(kind, Some(R_VOLUME | S_MUTE)).then_some(Msg::Wheel(Surface::Stage, event))
    })
}

/// The cover blurred across the window, or a plain wash without art.
fn backdrop(p: &mut P, now: &Now, w: f32, h: f32) {
    let pal = p.pal;
    match now.cover.art() {
        Some(art) => {
            let frame = art.backdrop(w / h.max(1.0), pal.dark);
            p.image(&frame, 0.0, 0.0, w, h);
            // Settle the center so type reads everywhere.
            p.fill(
                0.0,
                0.0,
                w,
                h,
                with_alpha(pal.surface, if pal.dark { 0x38 } else { 0x40 }),
            );
            p.vgradient(
                0.0,
                h * 0.55,
                w,
                h * 0.45,
                with_alpha(pal.surface, 0),
                with_alpha(pal.surface, if pal.dark { 0x90 } else { 0x88 }),
                0.0,
            );
        }
        None => {
            let base = if now.has_track {
                tint(pal.tone(&now.album_or_title()), pal.dark)
            } else {
                pal.panel
            };
            p.vgradient(
                0.0,
                0.0,
                w,
                h,
                mix(base, pal.surface, 0.2),
                pal.surface,
                0.0,
            );
        }
    }
}

/// Lyrics, left aligned in their column, the current line large.
#[allow(clippy::too_many_arguments)]
fn lyrics(
    p: &mut P,
    cx: &ZsCanvasLayoutContext<'_>,
    now: &Now,
    x: f32,
    w: f32,
    top: f32,
    h: f32,
    hover: Option<u64>,
) {
    let pal = p.pal;
    let lines = now
        .lyrics
        .as_ref()
        .map(|l| l.document.lines.as_slice())
        .unwrap_or(&[]);
    if lines.is_empty() {
        p.text_ex(
            if now.has_track {
                now.lyric_placeholder()
            } else {
                "播放后在这里显示歌词"
            },
            x,
            top + h * 0.42 - 20.0,
            w,
            40.0,
            ACTIVE,
            pal.text2,
            HorizontalAlign::Start,
            TextWeight::Bold,
        );
        return;
    }
    let current = now.frame.as_ref().and_then(|f| f.line_index);
    let mut tops = Vec::with_capacity(lines.len());
    let mut y = 0.0;
    for line in lines {
        tops.push(y);
        y += LINE_H + SUB_H * line.translation.len().min(1) as f32;
    }
    let shift = now::lyric_shift(2, current, &tops);
    let origin = top + h * 0.40 - shift - LINE_H / 2.0;
    p.push_clip(x - 16.0, top, w + 32.0, h);
    for (i, line) in lines.iter().enumerate() {
        let ly = origin + tops[i];
        let block = LINE_H + SUB_H * line.translation.len().min(1) as f32;
        if ly + block < top || ly > top + h {
            continue;
        }
        let active = current == Some(i);
        let text = if line.text.trim().is_empty() {
            "♪"
        } else {
            line.text.as_str()
        };
        let region = rid(S_LINE, i as u32);
        if hover == Some(region) && !active {
            p.round(
                x - 14.0,
                ly + 4.0,
                w + 28.0,
                block - 8.0,
                10.0,
                with_alpha(pal.text, 0x10),
            );
        }
        p.hover(region, x - 14.0, ly + 4.0, w + 28.0, block - 8.0);
        if active {
            p.text_ex(
                text,
                x,
                ly,
                w,
                LINE_H,
                ACTIVE,
                with_alpha(pal.text, 0x5c),
                HorizontalAlign::Start,
                TextWeight::Bold,
            );
            let tw = measure(cx, text, ACTIVE, TextWeight::Bold).min(w);
            let sung = now::sung_width(cx, line, now.frame.as_deref(), ACTIVE, tw);
            if sung > 0.0 {
                p.push_clip(x - 2.0, ly, sung + 2.0, LINE_H);
                p.text_ex(
                    text,
                    x,
                    ly,
                    w,
                    LINE_H,
                    ACTIVE,
                    pal.text,
                    HorizontalAlign::Start,
                    TextWeight::Bold,
                );
                p.pop_clip();
            }
        } else {
            let distance = current.map(|c| c.abs_diff(i)).unwrap_or(2) as f32;
            let alpha = (0xa0 as f32 - distance * 18.0).clamp(0x30 as f32, 0xa0 as f32) as u8;
            p.text_ex(
                text,
                x,
                ly,
                w,
                LINE_H,
                IDLE,
                with_alpha(pal.text, alpha),
                HorizontalAlign::Start,
                TextWeight::Semibold,
            );
        }
        if let Some(translation) = line.translation.first() {
            p.text(
                translation,
                x,
                ly + LINE_H - 10.0,
                w,
                SUB_H,
                14.0,
                with_alpha(pal.text, if active { 0xb0 } else { 0x60 }),
            );
        }
    }
    p.pop_clip();
}

pub fn click(
    state: &mut State,
    _surface: Surface,
    region: u64,
    _double: bool,
    x: f32,
    y: f32,
    cx: &mut AppCx,
) {
    let (kind, index) = rsplit(region);
    match kind {
        S_COLLAPSE => state.ui.stage = false,
        S_PLAY => app::act_toggle_play(state),
        S_PREV => app::act_command(state, BackendCommand::Previous),
        S_NEXT => app::act_command(state, BackendCommand::Next),
        S_MODE => app::act_cycle_mode(state),
        S_LOVE => app::act_toggle_current_love(state),
        S_MORE => app::act_open_menu(state, MenuKind::NowPlaying, Surface::Stage, x, y - 8.0),
        S_MUTE => {
            let muted = state.bridge.store().snapshot.audio_output.preferences.muted;
            app::act_command(state, BackendCommand::SetMute { muted: !muted });
        }
        S_LINE => now::seek_to_line(state, index as usize),
        S_ARTIST => {
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
        k if (C_MIN..C_MIN + 3).contains(&k) => caption_click(k - C_MIN, cx),
        _ => {}
    }
    let _ = Dp::new(0.0);
}
