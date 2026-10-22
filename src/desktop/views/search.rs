//! 搜索: before a search, trending keywords and the search history; after
//! it, songs or playlists from the chosen platform.

use super::{
    PAD,
    collection::{TableView, table},
    covers::Covers,
    discover::{columns, playlist_card, segmented_width},
    empty_state, failure, scroller, shared_click, skeleton, surface,
};
use crate::{
    app::{self, Msg, PLATFORM_NAMES, PLATFORMS, Page, SearchTab, State, Surface, platform_index},
    bridge::{Job, Load},
    fmt::count,
    paint::{P, measure, rid, rsplit},
    theme::{Palette, glyph as g},
};
use lx_gpui_player::backend::BackendCommand;
use zsui::{Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext, column};

const S_PLATFORM: u32 = 1;
const S_TAB: u32 = 2;
const S_HOT: u32 = 3;
const S_HISTORY: u32 = 4;
const S_CLEAR: u32 = 5;
const S_LIST: u32 = 6;
const S_LIST_PLAY: u32 = 7;

const HEAD_H: f32 = 112.0;
const GAP: f32 = 20.0;

pub fn view(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Search);
    let platform = state.ui.platform;
    let tab = state.ui.search_tab;
    let (keyword, load, total, shown, hot, history, lists) = {
        let store = state.bridge.store();
        (
            store.search.keyword.clone(),
            store.search.load.clone(),
            store.search.total,
            store.search.tracks.len(),
            store.hot.get(&platform).cloned().unwrap_or_default(),
            store.history.clone(),
            store.found_lists.clone(),
        )
    };
    let landing = keyword.is_empty();
    let covers = Covers::new(state);
    let jobs = state.bridge.sender();
    let head_keyword = keyword.clone();
    let head_load = load.clone();
    let head = surface(Surface::Search, move |cx: &ZsCanvasLayoutContext<'_>| {
        let _cover_frame = covers.begin_frame(cx);
        let mut p = P::new(pal);
        p.fill_visible(cx, pal.surface);
        let w = cx.size().width.0;
        let inner = w - PAD * 2.0;
        let scale = cx.scale_factor();
        let seg_w = segmented_width(cx);
        let mut y = 0.0;
        if landing {
            super::page_title(&mut p, PAD, y, inner - seg_w - 20.0, "搜索");
        } else {
            p.heading(
                &format!("“{head_keyword}”"),
                PAD - 4.0,
                y,
                inner - seg_w - 20.0,
                44.0,
                28.0,
                pal.text,
            );
        }
        p.segmented(
            cx,
            S_PLATFORM,
            &PLATFORM_NAMES,
            platform_index(platform),
            w - PAD - seg_w,
            y + 6.0,
            hover,
        );
        y += 60.0;
        if landing {
            // Trending keywords, ranked in two columns.
            p.section(cx, "热门搜索", PAD, y, inner, None, hover);
            y += 42.0;
            if hot.is_empty() {
                for i in 0..10 {
                    let x = PAD + (i / 5) as f32 * ((inner - GAP) / 2.0 + GAP);
                    skeleton(
                        &mut p,
                        x,
                        y + (i % 5) as f32 * 44.0 + 14.0,
                        (inner - GAP) / 2.0 - 40.0,
                        14.0,
                        7.0,
                    );
                }
                y += 5.0 * 44.0 + 24.0;
            } else {
                let n = hot.len().min(20);
                let per = n.div_ceil(2);
                let col_w = (inner - GAP) / 2.0;
                for (i, word) in hot.iter().take(n).enumerate() {
                    let x = PAD + (i / per) as f32 * (col_w + GAP);
                    let ry = y + (i % per) as f32 * 44.0;
                    let region = rid(S_HOT, i as u32);
                    if hover == Some(region) {
                        p.round(x - 10.0, ry, col_w + 20.0, 44.0, 8.0, pal.hover);
                    }
                    let top = i < 3;
                    p.num(
                        &format!("{}", i + 1),
                        x,
                        ry,
                        30.0,
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
                        x + 36.0,
                        ry,
                        col_w - 44.0,
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
            if !history.is_empty() {
                p.section(
                    cx,
                    "搜索历史",
                    PAD,
                    y,
                    inner,
                    Some((rid(S_CLEAR, 0), "清空")),
                    hover,
                );
                y += 42.0;
                let mut x = PAD;
                for (i, word) in history.iter().take(24).enumerate() {
                    let cw = measure(cx, word, 12.5, TextWeight::Regular) + 26.0;
                    if x + cw > PAD + inner {
                        x = PAD;
                        y += 40.0;
                    }
                    p.chip(cx, rid(S_HISTORY, i as u32), word, x, y, false, hover);
                    x += cw + 8.0;
                }
                y += 48.0;
            }
            p.scene.set_extent_height(Dp::new(y + 24.0));
            return p.finish();
        }

        // Results: the tabs, then songs (in the table below) or playlists.
        let mut tx = PAD;
        for (i, label) in ["单曲", "歌单"].into_iter().enumerate() {
            let id = rid(S_TAB, i as u32);
            let on = (i == 0) == (tab == SearchTab::Songs);
            let lw = measure(cx, label, 14.0, TextWeight::Bold);
            p.text_ex(
                label,
                tx,
                y,
                lw + 4.0,
                30.0,
                14.0,
                if on || hover == Some(id) {
                    pal.text
                } else {
                    pal.text3
                },
                HorizontalAlign::Start,
                TextWeight::Bold,
            );
            if on {
                p.round(tx, y + 32.0, lw, 2.5, 1.25, pal.text);
            }
            p.hover(id, tx - 6.0, y, lw + 12.0, 36.0);
            tx += lw + 26.0;
        }
        let summary = match tab {
            SearchTab::Songs => match &head_load {
                Load::Running if shown == 0 => "搜索中…".to_string(),
                Load::Failed(_) => String::new(),
                _ => format!("共 {} 首", count(total as usize)),
            },
            SearchTab::Lists => match &lists.load {
                Load::Running if lists.items.is_empty() => "搜索中…".to_string(),
                Load::Failed(_) => String::new(),
                _ => format!("共 {} 个歌单", count(lists.total as usize)),
            },
        };
        p.text(&summary, tx + 4.0, y, 200.0, 30.0, 12.5, pal.text3);
        y += 44.0;
        if tab == SearchTab::Songs {
            if let Load::Failed(error) = &head_load {
                failure(&mut p, cx, PAD, y, inner, error, hover);
            }
            p.scene.set_extent_height(Dp::new(HEAD_H));
            return p.finish();
        }
        match (&lists.load, lists.items.is_empty()) {
            (Load::Failed(error), true) => {
                failure(&mut p, cx, PAD, y, inner, error, hover);
                y += 48.0;
            }
            (Load::Done, true) => {
                empty_state(
                    &mut p,
                    PAD,
                    y + 24.0,
                    inner,
                    g::SEARCH,
                    "没有找到歌单",
                    "换个关键词试试",
                );
                y += 180.0;
            }
            (_, true) => {
                let (cols, card) = columns(inner);
                for i in 0..cols {
                    skeleton(&mut p, PAD + i as f32 * (card + GAP), y, card, card, 10.0);
                }
                y += card + 64.0;
            }
            _ => {
                let (cols, card) = columns(inner);
                let row_h = card + 64.0;
                for (i, list) in lists.items.iter().enumerate() {
                    let x = PAD + (i % cols) as f32 * (card + GAP);
                    let cy = y + (i / cols) as f32 * row_h;
                    if !super::covers::visible(cx, cy, row_h) {
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
                        rid(S_LIST, i as u32),
                        Some(rid(S_LIST_PLAY, i as u32)),
                    );
                }
                y += lists.items.len().div_ceil(cols) as f32 * row_h;
                let visible = cx.visible_rect();
                if lists.load == Load::Done
                    && (lists.items.len() as u64) < lists.total
                    && visible.y.0 + visible.height.0 > y - row_h
                    && let Some(platform) = lists.platform
                {
                    let _ = jobs.send(Job::SearchPlaylists {
                        platform,
                        keyword: lists.keyword.clone(),
                        page: lists.page + 1,
                    });
                }
                y += 24.0;
            }
        }
        p.scene.set_extent_height(Dp::new(y + 24.0));
        p.finish()
    });
    let key = Page::Search.scroll_key();
    if landing || tab == SearchTab::Lists {
        return scroller(state, &key, head);
    }
    let rows = state.rows.rows(state, &Page::Search);
    let more = match &load {
        Load::Running if shown > 0 => Some("加载中".to_string()),
        Load::Done if (shown as u64) < total && shown > 0 => Some("加载更多".to_string()),
        _ => None,
    };
    let empty = match &load {
        Load::Running => ("搜索中", ""),
        Load::Failed(_) => ("暂时无法搜索", "稍后再试，或换一个平台"),
        _ => ("没有找到歌曲", "换个关键词或平台试试"),
    };
    let view = TableView::new(state, rows, empty, more, true);
    scroller(
        state,
        &key,
        column([head.height(Dp::new(HEAD_H)), table(state, view)]),
    )
}

pub fn click(state: &mut State, region: u64) {
    if shared_click(state, region) {
        return;
    }
    let (kind, index) = rsplit(region);
    let index = index as usize;
    match kind {
        S_PLATFORM => {
            if let Some(platform) = PLATFORMS.get(index).copied() {
                app::act_set_platform(state, platform);
            }
        }
        S_TAB => app::act_set_search_tab(
            state,
            if index == 1 {
                SearchTab::Lists
            } else {
                SearchTab::Songs
            },
        ),
        S_HOT => {
            let word = state
                .bridge
                .store()
                .hot
                .get(&state.ui.platform)
                .and_then(|w| w.get(index).cloned());
            if let Some(word) = word {
                app::act_search(state, word);
            }
        }
        S_HISTORY => {
            let word = state.bridge.store().history.get(index).cloned();
            if let Some(word) = word {
                app::act_search(state, word);
            }
        }
        S_CLEAR => {
            app::act_command(state, BackendCommand::ClearSearchHistory);
            app::act_job(state, Job::RefreshSearchHistory);
        }
        S_LIST | S_LIST_PLAY => {
            let found = {
                let store = state.bridge.store();
                store
                    .found_lists
                    .platform
                    .zip(store.found_lists.items.get(index).map(|l| l.id.clone()))
            };
            if let Some((platform, id)) = found {
                if kind == S_LIST_PLAY {
                    app::act_job(
                        state,
                        Job::PlayPlaylist {
                            platform,
                            id: id.clone(),
                        },
                    );
                }
                app::act_open_playlist(state, platform, id);
            }
        }
        _ => {}
    }
}
