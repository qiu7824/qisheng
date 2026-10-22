//! 发现: what is new and popular on the browsing platform — recommended
//! playlists, the charts, trending searches — and the local library's
//! albums. Switching the platform in the header changes every online part.

use super::{
    PAD, R_ART_ON, R_RETRY, art_banner,
    covers::{self, Covers},
    failure, plays, scroller, shared_click, skeleton, surface,
};
use crate::{
    app::{self, GroupKind, PLATFORM_NAMES, PLATFORMS, Page, State, Surface, platform_index},
    bridge::{Job, Load},
    paint::{P, measure_num, rid, rsplit},
    theme::{Palette, glyph as g, with_alpha},
};
use lx_gpui_player::platforms::{Chart, Playlist};
use zsui::{Color, Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext};

const D_PLATFORM: u32 = 1;
const D_LIST: u32 = 2;
const D_LIST_PLAY: u32 = 3;
const D_CHART: u32 = 4;
const D_HOT: u32 = 5;
const D_ALBUM: u32 = 6;
const D_ALBUM_PLAY: u32 = 7;
const D_MORE_LISTS: u32 = 8;
const D_MORE_CHARTS: u32 = 9;
const D_MORE_ALBUMS: u32 = 10;

const GAP: f32 = 20.0;
const MIN_CARD: f32 = 148.0;

/// Card columns for an inner width.
pub fn columns(inner: f32) -> (usize, f32) {
    let cols = (((inner + GAP) / (MIN_CARD + GAP)).floor() as usize).clamp(2, 8);
    let card = (inner - GAP * (cols - 1) as f32) / cols as f32;
    (cols, card)
}

pub fn view(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Discover);
    let browse = state.ui.browse;
    let covers = Covers::new(state);
    let (wall, charts, charts_load, hot, auto) = {
        let store = state.bridge.store();
        (
            store
                .walls
                .get(&(browse, String::new()))
                .cloned()
                .unwrap_or_default(),
            store.chart_list.get(&browse).cloned().unwrap_or_default(),
            store.charts.get(&browse).cloned().unwrap_or(Load::Running),
            store.hot.get(&browse).cloned(),
            store.auto_art,
        )
    };
    let albums = state.groups.groups(state, GroupKind::Album);
    let content = surface(Surface::Discover, move |cx: &ZsCanvasLayoutContext<'_>| {
        let _cover_frame = covers.begin_frame(cx);
        let mut p = P::new(pal);
        p.fill_visible(cx, pal.surface);
        let w = cx.size().width.0;
        let inner = w - PAD * 2.0;
        let scale = cx.scale_factor();
        let mut y = 0.0;

        // Header.
        super::page_title(&mut p, PAD, y, inner - 300.0, "发现");
        let seg_w = segmented_width(cx);
        p.segmented(
            cx,
            D_PLATFORM,
            &PLATFORM_NAMES,
            platform_index(browse),
            w - PAD - seg_w,
            y + 6.0,
            hover,
        );
        y += 68.0;
        if !auto {
            y += art_banner(&mut p, cx, PAD, y, inner, hover);
        }

        // Recommended playlists.
        p.section(
            cx,
            "推荐歌单",
            PAD,
            y,
            inner,
            Some((rid(D_MORE_LISTS, 0), "更多歌单")),
            hover,
        );
        y += 42.0;
        let (cols, card) = columns(inner);
        let shown = (cols * 2).min(16);
        match (&wall.load, wall.items.is_empty()) {
            (Load::Failed(error), true) => {
                y += failure(&mut p, cx, PAD, y, inner, error, hover) + 16.0;
            }
            (_, true) => {
                for i in 0..shown {
                    let x = PAD + (i % cols) as f32 * (card + GAP);
                    let cy = y + (i / cols) as f32 * (card + 64.0);
                    skeleton(&mut p, x, cy, card, card, 10.0);
                    skeleton(&mut p, x, cy + card + 12.0, card * 0.8, 12.0, 6.0);
                }
                y += 2.0 * (card + 64.0);
            }
            _ => {
                for (i, list) in wall.items.iter().take(shown).enumerate() {
                    let x = PAD + (i % cols) as f32 * (card + GAP);
                    let cy = y + (i / cols) as f32 * (card + 64.0);
                    if !covers::visible(cx, cy, card + 64.0) {
                        continue;
                    }
                    playlist_card(
                        &mut p,
                        cx,
                        list,
                        covers.url(list.image_url.as_deref()).as_deref(),
                        x,
                        cy,
                        card,
                        scale,
                        hover,
                        rid(D_LIST, i as u32),
                        Some(rid(D_LIST_PLAY, i as u32)),
                    );
                }
                let rows = wall.items.len().min(shown).div_ceil(cols);
                y += rows as f32 * (card + 64.0);
            }
        }
        y += 20.0;

        // Charts.
        p.section(
            cx,
            "排行榜",
            PAD,
            y,
            inner,
            Some((rid(D_MORE_CHARTS, 0), "全部榜单")),
            hover,
        );
        y += 42.0;
        match (&charts_load, charts.is_empty()) {
            (Load::Failed(error), _) => {
                y += failure(&mut p, cx, PAD, y, inner, error, hover) + 16.0;
            }
            (_, true) => {
                for i in 0..cols {
                    let x = PAD + i as f32 * (card + GAP);
                    skeleton(&mut p, x, y, card, card * 0.62, 10.0);
                }
                y += card * 0.62 + 10.0;
            }
            _ => {
                // A caption line only when a platform describes its charts.
                let captions = charts
                    .iter()
                    .take(cols)
                    .any(|chart| caption(chart).is_some());
                let below = if captions { 34.0 } else { 0.0 };
                for (i, chart) in charts.iter().take(cols).enumerate() {
                    let x = PAD + i as f32 * (card + GAP);
                    if !covers::visible(cx, y, card * 0.62 + below) {
                        continue;
                    }
                    chart_tile(
                        &mut p,
                        chart,
                        covers.url(chart.image_url.as_deref()).as_deref(),
                        x,
                        y,
                        card,
                        scale,
                        hover == Some(rid(D_CHART, i as u32)),
                        i,
                    );
                    p.hover(rid(D_CHART, i as u32), x, y, card, card * 0.62 + below);
                }
                y += card * 0.62 + below + 10.0;
            }
        }
        y += 20.0;

        // Trending searches.
        if let Some(words) = &hot
            && !words.is_empty()
        {
            p.section(cx, "热门搜索", PAD, y, inner, None, hover);
            y += 42.0;
            let n = words.len().min(10);
            let per = n.div_ceil(2);
            let col_w = (inner - GAP) / 2.0;
            for (i, word) in words.iter().take(n).enumerate() {
                let x = PAD + (i / per) as f32 * (col_w + GAP);
                let ry = y + (i % per) as f32 * 44.0;
                let region = rid(D_HOT, i as u32);
                if hover == Some(region) {
                    p.round(x - 10.0, ry, col_w + 20.0, 44.0, 8.0, pal.hover);
                }
                let top = i < 3;
                p.num(
                    &format!("{}", i + 1),
                    x,
                    ry,
                    28.0,
                    44.0,
                    17.0,
                    if top { pal.text } else { pal.text3 },
                    HorizontalAlign::Start,
                    if top {
                        TextWeight::Bold
                    } else {
                        TextWeight::Regular
                    },
                );
                p.text_ex(
                    word,
                    x + 34.0,
                    ry,
                    col_w - 40.0,
                    44.0,
                    14.0,
                    pal.text,
                    HorizontalAlign::Start,
                    if top {
                        TextWeight::Semibold
                    } else {
                        TextWeight::Regular
                    },
                );
                p.hover(region, x - 10.0, ry, col_w + 20.0, 44.0);
            }
            y += per as f32 * 44.0 + 28.0;
        }

        // Local albums.
        if !albums.is_empty() {
            p.section(
                cx,
                "本地专辑",
                PAD,
                y,
                inner,
                Some((rid(D_MORE_ALBUMS, 0), "全部专辑")),
                hover,
            );
            y += 42.0;
            for (i, album) in albums.iter().take(cols).enumerate() {
                if !covers::visible(cx, y, card + 64.0) {
                    continue;
                }
                let x = PAD + i as f32 * (card + GAP);
                let region = rid(D_ALBUM, i as u32);
                let hot = hover == Some(region) || hover == Some(rid(D_ALBUM_PLAY, i as u32));
                covers::draw(
                    &mut p,
                    covers.track(&album.first_id).as_deref(),
                    x,
                    y,
                    card,
                    10.0,
                    scale,
                    &album.name,
                );
                if hot {
                    hover_play(&mut p, x, y, card, rid(D_ALBUM_PLAY, i as u32), hover);
                }
                p.text_b(&album.name, x, y + card + 10.0, card, 20.0, 13.0, pal.text);
                p.text(
                    &album.detail,
                    x,
                    y + card + 30.0,
                    card,
                    18.0,
                    12.0,
                    pal.text3,
                );
                p.hover(region, x, y, card, card + 50.0);
                if hot {
                    // Keep the play button above the card region.
                    p.hover(
                        rid(D_ALBUM_PLAY, i as u32),
                        x + card - 52.0,
                        y + card - 52.0,
                        42.0,
                        42.0,
                    );
                }
            }
            y += card + 72.0;
        }
        p.scene.set_extent_height(Dp::new(y + 24.0));
        let _ = (R_ART_ON, R_RETRY);
        p.finish()
    });
    scroller(state, &Page::Discover.scroll_key(), content)
}

use crate::app::Msg;

pub fn segmented_width(cx: &ZsCanvasLayoutContext<'_>) -> f32 {
    PLATFORM_NAMES
        .iter()
        .map(|l| crate::paint::measure(cx, l, 12.5, TextWeight::Semibold) + 24.0)
        .sum::<f32>()
        + 6.0
}

/// A darkened cover with a round play button in its corner.
pub fn hover_play(p: &mut P, x: f32, y: f32, size: f32, id: u64, hover: Option<u64>) {
    let pal = p.pal;
    p.round(x, y, size, size, 10.0, Color::rgba(0, 0, 0, 0x26));
    let (bx, by) = (x + size - 31.0, y + size - 31.0);
    let hot = hover == Some(id);
    p.shadow_ex(bx - 21.0, by - 21.0, 42.0, 42.0, 21.0, 0x50, 4.0, 8.0);
    p.circle(bx, by, if hot { 22.0 } else { 21.0 }, pal.white);
    p.glyph(
        g::PLAY,
        bx - 20.0,
        by - 20.0,
        41.0,
        40.0,
        13.0,
        Color::rgb(0x14, 0x14, 0x17),
    );
}

/// A playlist card: the cover with its play count, the name below in two
/// lines. `play` adds a play button on hover.
#[allow(clippy::too_many_arguments)]
pub fn playlist_card(
    p: &mut P,
    cx: &ZsCanvasLayoutContext<'_>,
    list: &Playlist,
    art: Option<&crate::art::ArtImage>,
    x: f32,
    y: f32,
    size: f32,
    scale: f32,
    hover: Option<u64>,
    region: u64,
    play: Option<u64>,
) {
    let pal = p.pal;
    let hot = hover == Some(region) || (play.is_some() && hover == play);
    covers::draw(p, art, x, y, size, 10.0, scale, &list.name);
    if let Some(count) = list.play_count.filter(|c| *c > 0) {
        let label = plays(count);
        let lw = measure_num(cx, &label, 11.0, TextWeight::Regular) + 30.0;
        let (bx, by) = (x + size - lw - 8.0, y + 8.0);
        p.round(bx, by, lw, 22.0, 11.0, Color::rgba(0, 0, 0, 0x70));
        p.glyph(g::PLAY, bx + 5.0, by, 14.0, 22.0, 7.0, pal.white);
        p.num(
            &label,
            bx + 21.0,
            by,
            lw - 22.0,
            22.0,
            11.0,
            pal.white,
            HorizontalAlign::Start,
            TextWeight::Regular,
        );
    }
    if hot && let Some(play) = play {
        hover_play(p, x, y, size, play, hover);
    }
    let style = crate::paint::para_style(13.0, HorizontalAlign::Start, TextWeight::Regular);
    p.para(
        &list.name,
        x,
        y + size + 10.0,
        size,
        40.0,
        13.0,
        style,
        if hot { pal.text } else { mixed_text(pal) },
    );
    p.hover(region, x, y, size, size + 50.0);
    if hot && let Some(play) = play {
        p.hover(play, x + size - 52.0, y + size - 52.0, 42.0, 42.0);
        p.tip(play, "播放歌单");
    }
}

fn mixed_text(pal: Palette) -> Color {
    crate::theme::mix(pal.text, pal.text2, 0.25)
}

/// A chart tile: a wide cover band with the chart's name set over a shade,
/// the rank of the chart in the corner.
#[allow(clippy::too_many_arguments)]
fn caption(chart: &Chart) -> Option<&str> {
    chart
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
}

pub fn chart_tile(
    p: &mut P,
    chart: &Chart,
    art: Option<&crate::art::ArtImage>,
    x: f32,
    y: f32,
    w: f32,
    scale: f32,
    hot: bool,
    index: usize,
) {
    let pal = p.pal;
    let h = w * 0.62;
    match art {
        Some(image) => {
            // Crop the square art into the band.
            p.push_clip(x, y, w, h);
            covers::image_at(p, image, x, y - (w - h) / 2.0, w, 0.0, scale);
            p.pop_clip();
            p.vgradient(
                x,
                y + h * 0.35,
                w,
                h * 0.65,
                Color::rgba(0, 0, 0, 0),
                Color::rgba(0, 0, 0, 0xa0),
                0.0,
            );
            p.text_ex(
                &chart.name,
                x + 14.0,
                y + h - 40.0,
                w - 28.0,
                30.0,
                (w * 0.1).clamp(14.0, 20.0),
                pal.white,
                HorizontalAlign::Start,
                TextWeight::Bold,
            );
            band_corners(p, x, y, w, h, pal.surface);
        }
        None => {
            let tone = pal.tone(&chart.name);
            p.round(x, y, w, h, 10.0, tone);
            p.num(
                &format!("{:02}", index + 1),
                x + 14.0,
                y + 10.0,
                60.0,
                24.0,
                13.0,
                with_alpha(pal.tone_ink, 0x80),
                HorizontalAlign::Start,
                TextWeight::Semibold,
            );
            p.text_ex(
                &chart.name,
                x + 14.0,
                y + h - 44.0,
                w - 28.0,
                34.0,
                (w * 0.115).clamp(15.0, 22.0),
                pal.tone_ink,
                HorizontalAlign::Start,
                TextWeight::Bold,
            );
        }
    }
    if hot {
        p.round(x, y, w, h, 10.0, Color::rgba(0, 0, 0, 0x1e));
    }
    if let Some(detail) = caption(chart) {
        p.text(detail, x, y + h + 8.0, w, 20.0, 12.0, pal.text3);
    }
}

/// Rounds a band drawn with square corners.
fn band_corners(p: &mut P, x: f32, y: f32, w: f32, h: f32, background: Color) {
    use zsui::{NativeDrawFill, ZsCanvasPoint, ZsCanvasPrimitive};
    let radius = 10.0f32;
    let point = |px: f32, py: f32| ZsCanvasPoint::new(Dp::new(px), Dp::new(py));
    let corners = [
        ((x, y), (x + radius, y + radius), 180.0f32),
        ((x + w, y), (x + w - radius, y + radius), 270.0),
        ((x + w, y + h), (x + w - radius, y + h - radius), 0.0),
        ((x, y + h), (x + radius, y + h - radius), 90.0),
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

pub fn click(state: &mut State, region: u64) {
    if shared_click(state, region) {
        return;
    }
    let (kind, index) = rsplit(region);
    let index = index as usize;
    let browse = state.ui.browse;
    match kind {
        D_PLATFORM => {
            if let Some(platform) = PLATFORMS.get(index).copied() {
                app::act_set_browse(state, platform);
            }
        }
        D_LIST | D_LIST_PLAY => {
            let id = state
                .bridge
                .store()
                .walls
                .get(&(browse, String::new()))
                .and_then(|w| w.items.get(index).map(|l| l.id.clone()));
            if let Some(id) = id {
                if kind == D_LIST_PLAY {
                    app::act_job(
                        state,
                        Job::PlayPlaylist {
                            platform: browse,
                            id: id.clone(),
                        },
                    );
                }
                app::act_open_playlist(state, browse, id);
            }
        }
        D_CHART => app::act_open_chart(state, index),
        D_HOT => {
            let word = state
                .bridge
                .store()
                .hot
                .get(&browse)
                .and_then(|words| words.get(index).cloned());
            if let Some(word) = word {
                app::act_search(state, word);
            }
        }
        D_ALBUM | D_ALBUM_PLAY => {
            let name = state
                .groups
                .groups(state, GroupKind::Album)
                .get(index)
                .map(|g| g.name.clone());
            if let Some(name) = name {
                if kind == D_ALBUM_PLAY {
                    state.play_group(GroupKind::Album, &name);
                } else {
                    app::act_navigate(state, Page::Group(GroupKind::Album, name));
                }
            }
        }
        D_MORE_LISTS => app::act_set_tag(state, String::new()),
        D_MORE_CHARTS => app::act_navigate(state, Page::Charts),
        D_MORE_ALBUMS => app::act_navigate(state, Page::Albums),
        _ => {}
    }
}
