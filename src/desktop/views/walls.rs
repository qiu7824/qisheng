//! Card walls: the playlist square of the browsing platform, and the albums
//! and artists of the local library.

use super::{
    PAD, art_banner,
    covers::{self, Covers},
    discover::{columns, hover_play, playlist_card, segmented_width},
    empty_state, failure, scroller, shared_click, skeleton, surface,
};
use crate::{
    app::{
        self, GroupKind, MenuKind, Msg, PLATFORM_NAMES, PLATFORMS, Page, State, Surface,
        platform_index,
    },
    bridge::{Job, Load, PLAYLIST_PAGE},
    fmt::count,
    paint::{P, rid, rsplit},
    theme::{Palette, glyph as g},
};
use std::sync::{Arc, mpsc};
use zsui::{Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext};

const W_PLATFORM: u32 = 1;
const W_TAG: u32 = 2;
const W_TAGS: u32 = 3;
const W_LIST: u32 = 4;
const W_LIST_PLAY: u32 = 5;
const W_ALBUM: u32 = 6;
const W_ALBUM_PLAY: u32 = 7;
const W_ARTIST: u32 = 8;

const GAP: f32 = 20.0;

/// The categories shown as chips: the hot ones when the platform marks
/// them, otherwise the first of each group.
fn chip_tags(tags: &[lx_gpui_player::platforms::PlaylistTag]) -> Vec<(String, String)> {
    let hot: Vec<_> = tags.iter().filter(|t| t.group == "热门").collect();
    let pick: Vec<_> = if hot.len() >= 6 {
        hot
    } else {
        tags.iter().collect()
    };
    pick.into_iter()
        .take(14)
        .map(|t| (t.id.clone(), t.name.clone()))
        .collect()
}

pub fn playlists(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Wall);
    let browse = state.ui.browse;
    let tag = state.ui.tag.clone();
    let covers = Covers::new(state);
    let jobs = state.bridge.sender();
    let (wall, tags, auto) = {
        let store = state.bridge.store();
        (
            store
                .walls
                .get(&(browse, tag.clone()))
                .cloned()
                .unwrap_or_default(),
            chip_tags(
                store
                    .tag_list
                    .get(&browse)
                    .map(|t| t.as_slice())
                    .unwrap_or(&[]),
            ),
            store.auto_art,
        )
    };
    let content = surface(Surface::Wall, move |cx: &ZsCanvasLayoutContext<'_>| {
        let _cover_frame = covers.begin_frame(cx);
        let mut p = P::new(pal);
        p.fill_visible(cx, pal.surface);
        let w = cx.size().width.0;
        let inner = w - PAD * 2.0;
        let scale = cx.scale_factor();
        let mut y = 0.0;
        super::page_title(&mut p, PAD, y, inner - 300.0, "歌单广场");
        let seg_w = segmented_width(cx);
        p.segmented(
            cx,
            W_PLATFORM,
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

        // Categories: two lines of chips at most, then the full menu.
        let mut x = PAD;
        let mut lines = 1;
        let all = [(String::new(), "推荐".to_string())]
            .into_iter()
            .chain(tags.iter().cloned());
        for (i, (id, name)) in all.enumerate() {
            let cw = crate::paint::measure(cx, &name, 12.5, TextWeight::Regular) + 26.0;
            if x + cw > PAD + inner - 100.0 {
                if lines == 2 {
                    break;
                }
                lines += 1;
                x = PAD;
                y += 40.0;
            }
            p.chip(cx, rid(W_TAG, i as u32), &name, x, y, id == tag, hover);
            x += cw + 8.0;
        }
        let more = rid(W_TAGS, 0);
        let hot = hover == Some(more);
        let label = "全部分类";
        let mw = crate::paint::measure(cx, label, 12.5, TextWeight::Regular) + 40.0;
        p.round(
            x,
            y,
            mw,
            30.0,
            15.0,
            if hot { pal.pressed } else { pal.surface },
        );
        p.round_stroke(
            x,
            y,
            mw,
            30.0,
            15.0,
            if hot { pal.pressed } else { pal.surface },
            pal.line_strong,
        );
        p.text(label, x + 13.0, y, mw - 30.0, 30.0, 12.5, pal.text2);
        p.glyph(
            g::CHEVRON_DOWN,
            x + mw - 26.0,
            y,
            16.0,
            30.0,
            8.5,
            pal.text3,
        );
        p.hover(more, x, y, mw, 30.0);
        y += 54.0;

        let (cols, card) = columns(inner);
        let row_h = card + 64.0;
        match (&wall.load, wall.items.is_empty()) {
            (Load::Failed(error), true) => {
                failure(&mut p, cx, PAD, y, inner, error, hover);
                y += 60.0;
            }
            (Load::Done, true) => {
                empty_state(
                    &mut p,
                    PAD,
                    y + 24.0,
                    inner,
                    g::GRID,
                    "这个分类还没有歌单",
                    "换个分类看看",
                );
                y += 180.0;
            }
            (_, true) => {
                for i in 0..cols * 2 {
                    let x = PAD + (i % cols) as f32 * (card + GAP);
                    let cy = y + (i / cols) as f32 * row_h;
                    skeleton(&mut p, x, cy, card, card, 10.0);
                    skeleton(&mut p, x, cy + card + 12.0, card * 0.8, 12.0, 6.0);
                }
                y += 2.0 * row_h;
            }
            _ => {
                let visible = cx.visible_rect();
                for (i, list) in wall.items.iter().enumerate() {
                    let x = PAD + (i % cols) as f32 * (card + GAP);
                    let cy = y + (i / cols) as f32 * row_h;
                    if cy + row_h < visible.y.0 || cy > visible.y.0 + visible.height.0 {
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
                        rid(W_LIST, i as u32),
                        Some(rid(W_LIST_PLAY, i as u32)),
                    );
                }
                let rows = wall.items.len().div_ceil(cols);
                y += rows as f32 * row_h;
                let more = (wall.items.len() as u64) < wall.total && wall.total > 0;
                if wall.load == Load::Running {
                    p.spinner(
                        w / 2.0 - 12.0,
                        y + 8.0,
                        24.0,
                        crate::fmt::phase_ms(),
                        pal.text2,
                        pal.raised,
                    );
                    y += 48.0;
                } else if more {
                    // Ask for the next page as the end comes into view.
                    if visible.y.0 + visible.height.0 > y - row_h * 1.5 {
                        request(&jobs, browse, &tag, wall.page + 1);
                    }
                    y += 48.0;
                }
            }
        }
        p.scene.set_extent_height(Dp::new(y + 24.0));
        p.finish()
    });
    scroller(state, &Page::Playlists.scroll_key(), content)
}

fn request(
    jobs: &mpsc::Sender<Job>,
    platform: lx_gpui_player::platforms::Platform,
    tag: &str,
    page: u32,
) {
    let _ = jobs.send(Job::Playlists {
        platform,
        tag: tag.to_string(),
        page,
    });
    let _ = PLAYLIST_PAGE;
}

pub fn albums(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Wall);
    let groups = state.groups.groups(state, GroupKind::Album);
    let covers = Covers::new(state);
    let songs = state.bridge.store().snapshot.tracks.len();
    let content = surface(Surface::Wall, move |cx: &ZsCanvasLayoutContext<'_>| {
        let _cover_frame = covers.begin_frame(cx);
        let mut p = P::new(pal);
        p.fill_visible(cx, pal.surface);
        let w = cx.size().width.0;
        let inner = w - PAD * 2.0;
        let scale = cx.scale_factor();
        let mut y = 0.0;
        super::page_title(&mut p, PAD, y, inner, "专辑");
        p.text(
            &format!("{} 张专辑，{} 首歌曲", count(groups.len()), count(songs)),
            PAD,
            y + 44.0,
            inner,
            20.0,
            12.5,
            pal.text3,
        );
        y += 84.0;
        if groups.is_empty() {
            empty_state(
                &mut p,
                PAD,
                y + 24.0,
                inner,
                g::ALBUM,
                "还没有专辑",
                "导入音乐或添加文件夹后，专辑会出现在这里",
            );
            p.scene.set_extent_height(Dp::new(y + 220.0));
            return p.finish();
        }
        let (cols, card) = columns(inner);
        let row_h = card + 64.0;
        let visible = cx.visible_rect();
        for (i, group) in groups.iter().enumerate() {
            let x = PAD + (i % cols) as f32 * (card + GAP);
            let cy = y + (i / cols) as f32 * row_h;
            if cy + row_h < visible.y.0 || cy > visible.y.0 + visible.height.0 {
                continue;
            }
            let region = rid(W_ALBUM, i as u32);
            let play = rid(W_ALBUM_PLAY, i as u32);
            let hot = hover == Some(region) || hover == Some(play);
            covers::draw(
                &mut p,
                covers.track(&group.first_id).as_deref(),
                x,
                cy,
                card,
                10.0,
                scale,
                &group.name,
            );
            if hot {
                hover_play(&mut p, x, cy, card, play, hover);
            }
            p.text_b(&group.name, x, cy + card + 10.0, card, 20.0, 13.0, pal.text);
            p.text(
                &group.detail,
                x,
                cy + card + 30.0,
                card,
                18.0,
                12.0,
                pal.text3,
            );
            p.hover(region, x, cy, card, card + 50.0);
            if hot {
                p.hover(play, x + card - 52.0, cy + card - 52.0, 42.0, 42.0);
                p.tip(play, "播放专辑");
            }
        }
        y += groups.len().div_ceil(cols) as f32 * row_h;
        p.scene.set_extent_height(Dp::new(y + 24.0));
        p.finish()
    });
    scroller(state, &Page::Albums.scroll_key(), content)
}

pub fn artists(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Wall);
    let groups = state.groups.groups(state, GroupKind::Artist);
    let content = surface(Surface::Wall, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        p.fill_visible(cx, pal.surface);
        let w = cx.size().width.0;
        let inner = w - PAD * 2.0;
        let mut y = 0.0;
        super::page_title(&mut p, PAD, y, inner, "歌手");
        p.text(
            &format!("{} 位歌手", count(groups.len())),
            PAD,
            y + 44.0,
            inner,
            20.0,
            12.5,
            pal.text3,
        );
        y += 84.0;
        if groups.is_empty() {
            empty_state(
                &mut p,
                PAD,
                y + 24.0,
                inner,
                g::ARTIST,
                "还没有歌手",
                "导入音乐或添加文件夹后，歌手会出现在这里",
            );
            p.scene.set_extent_height(Dp::new(y + 220.0));
            return p.finish();
        }
        let (cols, card) = columns(inner);
        let face = card * 0.84;
        let row_h = face + 64.0;
        let visible = cx.visible_rect();
        for (i, group) in groups.iter().enumerate() {
            let x = PAD + (i % cols) as f32 * (card + GAP);
            let cy = y + (i / cols) as f32 * row_h;
            if cy + row_h < visible.y.0 || cy > visible.y.0 + visible.height.0 {
                continue;
            }
            let region = rid(W_ARTIST, i as u32);
            let hot = hover == Some(region);
            let fx = x + (card - face) / 2.0;
            covers::avatar(&mut p, None, fx, cy, face, cx.scale_factor(), &group.name);
            if hot {
                p.round(
                    fx,
                    cy,
                    face,
                    face,
                    face / 2.0,
                    zsui::Color::rgba(0, 0, 0, 0x1e),
                );
            }
            p.text_ex(
                &group.name,
                x,
                cy + face + 10.0,
                card,
                20.0,
                13.0,
                pal.text,
                HorizontalAlign::Center,
                TextWeight::Semibold,
            );
            p.text_c(
                &format!("{} 首", group.count),
                x,
                cy + face + 30.0,
                card,
                18.0,
                12.0,
                pal.text3,
            );
            p.hover(region, x, cy, card, face + 50.0);
        }
        y += groups.len().div_ceil(cols) as f32 * row_h;
        p.scene.set_extent_height(Dp::new(y + 24.0));
        p.finish()
    });
    scroller(state, &Page::Artists.scroll_key(), content)
}

pub fn click(state: &mut State, region: u64, x: f32, y: f32) {
    if shared_click(state, region) {
        return;
    }
    let (kind, index) = rsplit(region);
    let index = index as usize;
    let browse = state.ui.browse;
    match kind {
        W_PLATFORM => {
            if let Some(platform) = PLATFORMS.get(index).copied() {
                app::act_set_browse(state, platform);
            }
        }
        W_TAG => {
            let tag = if index == 0 {
                Some(String::new())
            } else {
                let store = state.bridge.store();
                chip_tags(
                    store
                        .tag_list
                        .get(&browse)
                        .map(|t| t.as_slice())
                        .unwrap_or(&[]),
                )
                .get(index - 1)
                .map(|t| t.0.clone())
            };
            if let Some(tag) = tag {
                app::act_set_tag(state, tag);
            }
        }
        W_TAGS => app::act_open_menu(state, MenuKind::Tags, Surface::Wall, x, y + 18.0),
        W_LIST | W_LIST_PLAY => {
            let id = state
                .bridge
                .store()
                .walls
                .get(&(browse, state.ui.tag.clone()))
                .and_then(|w| w.items.get(index).map(|l| l.id.clone()));
            if let Some(id) = id {
                if kind == W_LIST_PLAY {
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
        W_ALBUM | W_ALBUM_PLAY => {
            let name = state
                .groups
                .groups(state, GroupKind::Album)
                .get(index)
                .map(|g| g.name.clone());
            if let Some(name) = name {
                if kind == W_ALBUM_PLAY {
                    state.play_group(GroupKind::Album, &name);
                } else {
                    app::act_navigate(state, Page::Group(GroupKind::Album, name));
                }
            }
        }
        W_ARTIST => {
            let name = state
                .groups
                .groups(state, GroupKind::Artist)
                .get(index)
                .map(|g| g.name.clone());
            if let Some(name) = name {
                app::act_navigate(state, Page::Group(GroupKind::Artist, name));
            }
        }
        _ => {}
    }
    let _ = Arc::new(());
}
