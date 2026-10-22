//! The navigation column: the brand and the search field, the places to
//! browse, the library with its counts, the user's playlists, and settings
//! and sources at the foot.

use super::{
    NAV_W, centered,
    covers::{self, Covers},
    layers, scroller, surface,
};
use crate::{
    app::{self, GroupKind, MenuKind, Msg, Page, State, Surface, W_SEARCH},
    bridge::list_display_name,
    paint::{P, measure_num, rid, rsplit},
    theme::{NUM_FONT, Palette, glyph as g},
};
use zsui::{
    AppCx, Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext, column, row, spacer,
    textbox,
};

const HEAD_H: f32 = 112.0;
const FOOT_H: f32 = 60.0;
const ITEM_H: f32 = 36.0;
const FIELD_Y: f32 = 62.0;
const FIELD_H: f32 = 36.0;
const X0: f32 = 12.0;

const N_ITEM: u32 = 1;
const N_LIST: u32 = 2;
const N_ADD: u32 = 3;
const N_FIELD: u32 = 4;
const N_SETTINGS: u32 = 5;
const N_SOURCES: u32 = 6;
const N_BRAND: u32 = 7;

/// The fixed places, in order.
fn places() -> [(char, char, &'static str, Page); 7] {
    [
        (g::HOME, g::HOME_FILL, "发现", Page::Discover),
        (g::GRID, g::GRID, "歌单广场", Page::Playlists),
        (g::CHART, g::CHART, "排行榜", Page::Charts),
        (g::NOTES, g::NOTES, "本地音乐", Page::Songs),
        (g::ALBUM, g::ALBUM, "专辑", Page::Albums),
        (g::ARTIST, g::ARTIST, "歌手", Page::Artists),
        (g::HEART, g::HEART_FILL, "我喜欢", Page::List("love".into())),
    ]
}

/// The user's own playlists shown in the navigation.
fn user_lists(state: &State) -> Vec<(String, String, usize)> {
    let store = state.bridge.store();
    store
        .lists
        .iter()
        .filter(|l| l.id != "temp" && l.id != "love")
        .map(|l| {
            (
                l.id.clone(),
                list_display_name(l),
                store.list_totals.get(&l.id).copied().unwrap_or(0),
            )
        })
        .collect()
}

/// The playlist id under a navigation region, for its context menu.
pub fn list_at(state: &State, region: u64) -> Option<String> {
    let (kind, index) = rsplit(region);
    (kind == N_LIST)
        .then(|| user_lists(state).get(index as usize).map(|l| l.0.clone()))
        .flatten()
}

/// Whether `page` belongs under a place (drill-down pages light their
/// parent).
fn is_place(page: &Page, place: &Page) -> bool {
    match (page, place) {
        (Page::Playlist(..), Page::Playlists) => true,
        (Page::Group(GroupKind::Album, _), Page::Albums) => true,
        (Page::Group(GroupKind::Artist, _), Page::Artists) => true,
        _ => page == place,
    }
}

pub fn view(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    super::backed(
        pal.nav,
        column([
            head(state),
            scroller(state, "nav", list(state)).flex(1.0),
            foot(state),
        ]),
    )
}

fn head(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::NavHead);
    let searching = state.ui.page == Page::Search;
    let canvas = surface(Surface::NavHead, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let w = cx.size().width.0;
        let scale = cx.scale_factor();
        p.fill(0.0, 0.0, w, HEAD_H, pal.nav);
        if let Some(logo) = crate::art::logo((22.0 * scale).round() as u32) {
            p.image(&logo, 20.0, 19.0, 22.0, 22.0);
        }
        p.text_ex(
            "栖声",
            50.0,
            16.0,
            w - 60.0,
            28.0,
            15.0,
            pal.text,
            HorizontalAlign::Start,
            TextWeight::Bold,
        );
        p.hover_plain(rid(N_BRAND, 0), 16.0, 14.0, 120.0, 32.0);
        let fx = X0 + 4.0;
        let fw = w - fx * 2.0;
        let hot = hover == Some(rid(N_FIELD, 0));
        if searching || hot {
            p.round_stroke(fx, FIELD_Y, fw, FIELD_H, 10.0, pal.raised, pal.line_strong);
        } else {
            p.round(fx, FIELD_Y, fw, FIELD_H, 10.0, pal.raised);
        }
        p.glyph(g::SEARCH, fx + 4.0, FIELD_Y, 32.0, FIELD_H, 12.0, pal.text3);
        p.hover_plain(rid(N_FIELD, 0), fx, FIELD_Y, 34.0, FIELD_H);
        p.finish()
    })
    .window_drag_region();
    let search = textbox(state.ui.query.clone())
        .id(W_SEARCH)
        .placeholder("搜索音乐")
        .on_change(Msg::Query)
        .on_submit(Msg::Submit)
        .on_focus_change(Msg::SearchFocus)
        .chromeless()
        .width(Dp::new(super::NAV_W - (X0 + 4.0 + 34.0) - (X0 + 10.0)))
        .height(Dp::new(30.0));
    let field = row([
        spacer().width(Dp::new(X0 + 4.0 + 34.0)),
        centered(search, FIELD_H).flex(1.0),
        spacer().width(Dp::new(X0 + 10.0)),
    ]);
    layers([
        canvas,
        column([
            spacer().height(Dp::new(FIELD_Y)),
            field.height(Dp::new(FIELD_H)),
            spacer().flex(1.0),
        ]),
    ])
    .height(Dp::new(HEAD_H))
}

fn list(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Nav);
    let page = state.ui.page.clone();
    let lists = user_lists(state);
    let (songs, love, origin, playing) = {
        let store = state.bridge.store();
        (
            store.snapshot.tracks.len(),
            store.list_totals.get("love").copied().unwrap_or(0),
            store.queue.as_ref().map(|q| q.origin_list.clone()),
            super::now::is_active(store.snapshot.playback.status),
        )
    };
    let albums = state.groups.groups(state, GroupKind::Album).len();
    let artists = state.groups.groups(state, GroupKind::Artist).len();
    let covers = Covers::new(state);
    // The first track of each loaded list stands for it.
    let firsts = {
        let store = state.bridge.store();
        lists
            .iter()
            .map(|(list_id, _, _)| {
                store
                    .list_data
                    .get(list_id)
                    .and_then(|d| d.tracks.first().cloned())
                    .map(|t| (list_id.clone(), t))
            })
            .collect::<Vec<_>>()
    };
    surface(Surface::Nav, move |cx: &ZsCanvasLayoutContext<'_>| {
        let _cover_frame = covers.begin_frame(cx);
        let mut p = P::new(pal);
        p.fill_visible(cx, pal.nav);
        let w = cx.size().width.0;
        let iw = w - X0 * 2.0;
        let scale = cx.scale_factor();
        let mut y = 4.0;
        let label = |p: &mut P, y: f32, text: &str| {
            p.text_ex(
                text,
                X0 + 12.0,
                y,
                iw - 40.0,
                28.0,
                12.0,
                pal.text3,
                HorizontalAlign::Start,
                TextWeight::Semibold,
            );
        };
        for (i, (glyph, glyph_on, title, target)) in places().into_iter().enumerate() {
            if i == 3 {
                y += 14.0;
                label(&mut p, y, "我的音乐");
                y += 30.0;
            }
            let region = rid(N_ITEM, i as u32);
            let on = is_place(&page, &target);
            let hot = hover == Some(region);
            if on {
                p.round(X0, y, iw, ITEM_H, 8.0, pal.pressed);
            } else if hot {
                p.round(X0, y, iw, ITEM_H, 8.0, pal.hover);
            }
            let color = if on || hot { pal.text } else { pal.text2 };
            p.glyph(
                if on { glyph_on } else { glyph },
                X0 + 8.0,
                y,
                24.0,
                ITEM_H,
                13.0,
                if on && i == 6 { pal.love } else { color },
            );
            p.text_ex(
                title,
                X0 + 40.0,
                y,
                iw - 84.0,
                ITEM_H,
                13.5,
                color,
                HorizontalAlign::Start,
                if on {
                    TextWeight::Semibold
                } else {
                    TextWeight::Regular
                },
            );
            let count = match i {
                3 => songs,
                4 => albums,
                5 => artists,
                6 => love,
                _ => 0,
            };
            let plays_here = playing
                && match i {
                    3 => origin.as_deref() == Some("catalog"),
                    6 => origin.as_deref() == Some("love"),
                    _ => false,
                };
            if plays_here {
                super::bars(
                    &mut p,
                    X0 + iw - 26.0,
                    y + ITEM_H / 2.0,
                    true,
                    crate::fmt::phase_ms(),
                    pal.text2,
                );
            } else if count > 0 {
                p.num(
                    &count.to_string(),
                    X0 + iw - 52.0,
                    y,
                    40.0,
                    ITEM_H,
                    12.0,
                    pal.text3,
                    HorizontalAlign::End,
                    TextWeight::Regular,
                );
            }
            p.hover(region, X0, y, iw, ITEM_H);
            y += ITEM_H + 2.0;
        }

        // Playlists.
        y += 14.0;
        label(&mut p, y, "歌单");
        let add = rid(N_ADD, 0);
        p.tool(
            add,
            g::ADD,
            X0 + iw - 14.0,
            y + 14.0,
            26.0,
            10.0,
            hover,
            false,
            "新建歌单或导入",
        );
        y += 32.0;
        if lists.is_empty() {
            p.text("还没有歌单", X0 + 12.0, y, iw - 24.0, 28.0, 12.5, pal.text3);
            y += 32.0;
        }
        for (i, (id, name, total)) in lists.iter().enumerate() {
            if !covers::visible(cx, y, 44.0) {
                y += 46.0;
                continue;
            }
            let art =
                firsts
                    .get(i)
                    .and_then(|first| first.as_ref())
                    .and_then(|(list_id, track)| {
                        covers
                            .get(&track.id, list_id, track.platform.source == "local")
                            .or_else(|| covers.url(crate::rows::cover_url(track).as_deref()))
                    });
            let region = rid(N_LIST, i as u32);
            let on = page == Page::List(id.clone());
            let hot = hover == Some(region);
            if on {
                p.round(X0, y, iw, 44.0, 8.0, pal.pressed);
            } else if hot {
                p.round(X0, y, iw, 44.0, 8.0, pal.hover);
            }
            covers::draw(
                &mut p,
                art.as_deref(),
                X0 + 8.0,
                y + 6.0,
                32.0,
                5.0,
                scale,
                name,
            );
            let color = if on || hot { pal.text } else { pal.text2 };
            p.text_ex(
                name,
                X0 + 50.0,
                y + 4.0,
                iw - 62.0,
                20.0,
                13.0,
                color,
                HorizontalAlign::Start,
                if on {
                    TextWeight::Semibold
                } else {
                    TextWeight::Regular
                },
            );
            let plays_here = playing && origin.as_deref() == Some(id.as_str());
            let detail = if plays_here {
                "正在播放".to_string()
            } else {
                format!("{total} 首")
            };
            p.text_family(
                &detail,
                X0 + 50.0,
                y + 23.0,
                iw - 62.0,
                16.0,
                11.5,
                pal.text3,
                HorizontalAlign::Start,
                TextWeight::Regular,
                (!plays_here).then_some(NUM_FONT),
            );
            p.hover(region, X0, y, iw, 44.0);
            y += 46.0;
        }
        let _ = measure_num;
        p.scene.set_extent_height(Dp::new(y + 16.0));
        p.finish()
    })
}

fn foot(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::NavFoot);
    let page = state.ui.page.clone();
    surface(Surface::NavFoot, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let w = cx.size().width.0;
        p.fill(0.0, 0.0, w, FOOT_H, pal.nav);
        p.hline(X0 + 4.0, 0.0, w - (X0 + 4.0) * 2.0, pal.line);
        let half = (w - X0 * 2.0 - 8.0) / 2.0;
        for (i, (kind, glyph, label, target)) in [
            (N_SETTINGS, g::SETTINGS, "设置", Page::Settings),
            (N_SOURCES, g::SOURCE, "音源", Page::Sources),
        ]
        .into_iter()
        .enumerate()
        {
            let x = X0 + i as f32 * (half + 8.0);
            let region = rid(kind, 0);
            let on = page == target;
            let hot = hover == Some(region);
            if on {
                p.round(x, 12.0, half, 36.0, 8.0, pal.pressed);
            } else if hot {
                p.round(x, 12.0, half, 36.0, 8.0, pal.hover);
            }
            let color = if on || hot { pal.text } else { pal.text2 };
            p.glyph(glyph, x + 8.0, 12.0, 24.0, 36.0, 12.5, color);
            p.text(label, x + 36.0, 12.0, half - 40.0, 36.0, 13.0, color);
            p.hover(region, x, 12.0, half, 36.0);
        }
        p.finish()
    })
    .height(Dp::new(FOOT_H))
}

pub fn click(state: &mut State, surface: Surface, region: u64, x: f32, y: f32, cx: &mut AppCx) {
    let (kind, index) = rsplit(region);
    match (surface, kind) {
        (Surface::NavHead, N_FIELD) => cx.focus(W_SEARCH),
        (Surface::NavHead, N_BRAND) => app::act_navigate(state, Page::Discover),
        (Surface::Nav, N_ITEM) => {
            if let Some((_, _, _, page)) = places().into_iter().nth(index as usize) {
                app::act_navigate(state, page);
            }
        }
        (Surface::Nav, N_LIST) => {
            if let Some((id, _, _)) = user_lists(state).get(index as usize).cloned() {
                app::act_navigate(state, Page::List(id));
            }
        }
        (Surface::Nav, N_ADD) => {
            app::act_open_menu(state, MenuKind::Create, Surface::Nav, x, y + 16.0);
        }
        (Surface::NavFoot, N_SETTINGS) => app::act_navigate(state, Page::Settings),
        (Surface::NavFoot, N_SOURCES) => app::act_navigate(state, Page::Sources),
        _ => {}
    }
    let _ = NAV_W;
}
