//! Collection pages — local music, an album, an artist, a playlist of the
//! user or of a platform, the open chart — share one layout: the cover
//! beside a small label, the title, a line of facts and the actions, then
//! the track table. Everything scrolls together. The table is also used by
//! search results.

use super::{PAD, bars, plays, scroller, shared_click, surface};
use crate::{
    app::{self, GroupKind, Menu, MenuKind, Msg, Page, State, Surface, platform_name, source_tag},
    bridge::{CHART_PAGE, Job, Load, PLAYLIST_TRACKS, list_display_name},
    fmt::{clock, count, span},
    paint::{Btn, P, measure, rid, rsplit},
    rows::{RowSource, Rows},
    theme::{NUM_FONT, Palette, glyph as g, with_alpha},
    views::covers::{self, Covers},
};
use lx_gpui_player::{backend::BackendCommand, platforms::Platform};
use std::{collections::HashMap, sync::Arc};
use zsui::{
    Command, Dp, HorizontalAlign, MenuItemSpec, MenuSpec, TextWeight, ViewNode,
    ZsCanvasLayoutContext, column,
};

pub const ROW_H: f32 = 56.0;
const TABLE_HEAD: f32 = 40.0;
const HERO_H: f32 = 236.0;
const BUTTON_H: f32 = 38.0;

const R_ROW: u32 = 20;
const R_PLAY: u32 = 21;
const R_LOVE: u32 = 22;
const R_MORE: u32 = 23;
const R_MORE_PAGE: u32 = 24;

const A_PLAY: u32 = 40;
const A_SHUFFLE: u32 = 41;
const A_MORE: u32 = 42;
const A_SAVE: u32 = 43;
const A_ADD: u32 = 44;

pub fn is_row_region(kind: u32) -> bool {
    matches!(kind, R_ROW | R_PLAY | R_LOVE | R_MORE)
}

/// What a collection header shows.
#[derive(Clone)]
struct Info {
    eyebrow: String,
    title: String,
    meta: String,
    description: String,
    /// Stands in for a missing cover.
    glyph: Option<char>,
    round: bool,
    more: bool,
    save: bool,
    add: bool,
    show_tag: bool,
    ranked: bool,
    load_more: Option<String>,
    empty: (char, &'static str, &'static str),
    /// The queue origin of this collection, to show pause while it plays.
    origin: Option<String>,
    image: Option<String>,
}

fn info(state: &State, rows: &Rows) -> Info {
    let filtering = !state.ui.filter.trim().is_empty();
    let n = rows.total;
    let facts = |extra: Vec<String>| {
        let mut parts = extra;
        parts.push(format!("{} 首", count(n)));
        if n > 0 && !filtering {
            parts.push(span(rows.seconds()));
        }
        parts.retain(|p| !p.is_empty());
        parts.join(" · ")
    };
    let no_match = (g::SEARCH, "没有匹配的歌曲", "换个关键词试试");
    let base = Info {
        eyebrow: String::new(),
        title: String::new(),
        meta: String::new(),
        description: String::new(),
        glyph: None,
        round: false,
        more: false,
        save: false,
        add: false,
        show_tag: false,
        ranked: false,
        load_more: None,
        empty: (g::NOTE, "", ""),
        origin: None,
        image: None,
    };
    match &state.ui.page {
        Page::Songs => {
            let roots = state.bridge.store().library_roots.len();
            Info {
                eyebrow: "本地音乐".into(),
                title: "全部歌曲".into(),
                meta: facts(vec![if roots > 0 {
                    format!("{roots} 个文件夹")
                } else {
                    String::new()
                }]),
                add: true,
                empty: if filtering {
                    no_match
                } else {
                    (
                        g::FOLDER,
                        "曲库是空的",
                        "导入音乐文件，或者添加一个音乐文件夹",
                    )
                },
                origin: Some("catalog".into()),
                glyph: Some(g::NOTES),
                ..base
            }
        }
        Page::Group(kind, name) => {
            let detail = state
                .groups
                .groups(state, *kind)
                .iter()
                .find(|g| &g.name == name)
                .map(|g| g.detail.clone())
                .unwrap_or_default();
            Info {
                eyebrow: match kind {
                    GroupKind::Album => "专辑",
                    GroupKind::Artist => "歌手",
                }
                .into(),
                title: name.clone(),
                meta: facts(vec![detail]),
                round: *kind == GroupKind::Artist,
                empty: if filtering {
                    no_match
                } else {
                    (g::NOTE, "没有歌曲", "")
                },
                ..base
            }
        }
        Page::List(id) => {
            let (name, load) = {
                let store = state.bridge.store();
                (
                    store
                        .lists
                        .iter()
                        .find(|l| &l.id == id)
                        .map(list_display_name)
                        .unwrap_or_else(|| "歌单".into()),
                    store
                        .list_data
                        .get(id)
                        .map(|d| d.load.clone())
                        .unwrap_or(Load::Running),
                )
            };
            Info {
                eyebrow: if id == "love" {
                    "我喜欢"
                } else {
                    "我的歌单"
                }
                .into(),
                meta: match &load {
                    Load::Failed(error) => error.clone(),
                    Load::Running if n == 0 => "读取中".into(),
                    _ => facts(Vec::new()),
                },
                title: name,
                glyph: (id == "love").then_some(g::HEART_FILL),
                more: true,
                show_tag: true,
                empty: match id.as_str() {
                    _ if filtering => no_match,
                    "love" => (g::HEART, "还没有喜欢的歌曲", "点歌曲旁的 ♡ 把它收进这里"),
                    _ => (g::QUEUE, "歌单是空的", "在歌曲上右键，选择「添加到歌单」"),
                },
                origin: Some(id.clone()),
                ..base
            }
        }
        Page::Playlist(platform, id) => {
            let store = state.bridge.store();
            let view = &store.playlist;
            let shown = view.tracks.len();
            let info = (&view.id == id).then(|| view.info.clone()).flatten();
            let total = if view.total > 0 {
                view.total as usize
            } else {
                shown
            };
            let mut parts = Vec::new();
            // Authors that are only account ids say nothing; skip them.
            if let Some(author) = info.as_ref().and_then(|i| i.author.clone())
                && !author.trim().is_empty()
                && author.chars().filter(|c| c.is_ascii_digit()).count() * 2
                    < author.chars().count()
            {
                parts.push(author);
            }
            if let Some(count) = info.as_ref().and_then(|i| i.play_count).filter(|c| *c > 0) {
                parts.push(format!("{} 次播放", plays(count)));
            }
            if total > 0 {
                parts.push(format!("{} 首", count(total)));
            }
            Info {
                eyebrow: format!("歌单 · {}", platform_name(*platform)),
                title: info
                    .as_ref()
                    .map(|i| i.name.clone())
                    .filter(|n| !n.trim().is_empty())
                    .unwrap_or_else(|| "歌单".into()),
                meta: match &view.load {
                    Load::Failed(error) if shown == 0 => error.clone(),
                    Load::Running if shown == 0 => "加载中".into(),
                    _ => parts.join(" · "),
                },
                description: info
                    .as_ref()
                    .and_then(|i| i.description.clone())
                    .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
                    .unwrap_or_default(),
                save: true,
                more: true,
                show_tag: false,
                load_more: match &view.load {
                    Load::Running if shown > 0 => Some("加载中".into()),
                    Load::Done if (shown as u64) < view.total && shown > 0 && !filtering => {
                        Some("加载更多".into())
                    }
                    _ => None,
                },
                empty: match &view.load {
                    Load::Failed(_) => (g::WARNING, "暂时无法加载这个歌单", "稍后再试"),
                    Load::Running => (g::NOTE, "加载中", ""),
                    _ if filtering => no_match,
                    _ => (g::NOTE, "歌单是空的", ""),
                },
                image: info.and_then(|i| i.image_url),
                ..base
            }
        }
        Page::Charts => {
            let store = state.bridge.store();
            let chart = &store.chart;
            let platform = chart.platform.unwrap_or(state.ui.browse);
            let known = store
                .chart_list
                .get(&platform)
                .and_then(|list| list.iter().find(|c| c.id == chart.id))
                .cloned();
            let shown = chart.tracks.len();
            Info {
                eyebrow: format!("排行榜 · {}", platform_name(platform)),
                title: known
                    .as_ref()
                    .map(|c| c.name.clone())
                    .unwrap_or_else(|| "排行榜".into()),
                meta: match &chart.load {
                    Load::Failed(_) => "暂时无法加载".into(),
                    Load::Running if shown == 0 => "加载中".into(),
                    _ => format!("{} 首", count(chart.total as usize)),
                },
                description: known
                    .as_ref()
                    .and_then(|c| c.description.clone())
                    .unwrap_or_default(),
                save: true,
                more: true,
                ranked: true,
                load_more: match &chart.load {
                    Load::Running if shown > 0 => Some("加载中".into()),
                    _ if (shown as u64) < chart.total && shown > 0 && !filtering => {
                        Some("加载更多".into())
                    }
                    _ => None,
                },
                empty: match chart.load {
                    Load::Failed(_) => (g::WARNING, "暂时无法加载这个榜单", "稍后再试"),
                    Load::Running => (g::NOTE, "加载中", ""),
                    _ if filtering => no_match,
                    _ => (g::CHART, "选一个榜单", ""),
                },
                image: known.and_then(|c| c.image_url),
                ..base
            }
        }
        _ => Info {
            show_tag: true,
            ..base
        },
    }
}

pub fn view(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let page = state.ui.page.clone();
    let rows = state.rows.rows(state, &page);
    let info = info(state, &rows);
    let (current_origin, playing) = {
        let store = state.bridge.store();
        (
            store.queue.as_ref().map(|q| q.origin_list.clone()),
            super::now::is_active(store.snapshot.playback.status),
        )
    };
    let plays_here = playing && info.origin.is_some() && info.origin == current_origin;
    let hover = state.hover(Surface::Hero);
    let hero_info = info.clone();
    let covers = Covers::new(state);
    // Albums show their first track's cover; lists their first tracks'.
    let firsts: Vec<Arc<crate::art::ArtImage>> = match &page {
        Page::Group(GroupKind::Album, name) => state
            .groups
            .groups(state, GroupKind::Album)
            .iter()
            .find(|g| &g.name == name)
            .and_then(|g| covers.track(&g.first_id))
            .into_iter()
            .collect(),
        Page::List(id) if id != "love" => rows
            .items
            .iter()
            .take(4)
            .filter_map(|row| covers.row(row, &rows.source))
            .collect(),
        Page::Playlist(..) | Page::Charts => {
            covers.url(info.image.as_deref()).into_iter().collect()
        }
        _ => Vec::new(),
    };
    let hero = surface(Surface::Hero, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let w = cx.size().width.0;
        let scale = cx.scale_factor();
        p.fill(0.0, 0.0, w, HERO_H, pal.surface);
        let compact = w < 620.0;
        let cover = if compact { 140.0 } else { 188.0 };
        let (sx, sy) = (PAD, 4.0 + (188.0 - cover) / 2.0);
        if !hero_info.round {
            p.shadow_ex(
                sx,
                sy,
                cover,
                cover,
                12.0,
                if pal.dark { 0x80 } else { 0x40 },
                8.0,
                18.0,
            );
        }
        match (hero_info.round, hero_info.glyph) {
            (true, _) => covers::avatar(&mut p, None, sx, sy, cover, scale, &hero_info.title),
            (false, Some(glyph)) if firsts.is_empty() => {
                covers::drawn(&mut p, sx, sy, cover, 12.0, &hero_info.title, Some(glyph))
            }
            _ => covers::mosaic(
                &mut p,
                &firsts,
                sx,
                sy,
                cover,
                12.0,
                scale,
                &hero_info.title,
                pal.surface,
            ),
        }

        let tx = sx + cover + 28.0;
        let tw = (w - tx - PAD).max(80.0);
        p.text_ex(
            &hero_info.eyebrow,
            tx,
            sy + 6.0,
            tw,
            20.0,
            12.5,
            pal.text3,
            HorizontalAlign::Start,
            TextWeight::Semibold,
        );
        let size = [30.0, 26.0, 22.0]
            .into_iter()
            .find(|size| measure(cx, &hero_info.title, *size, TextWeight::Bold) <= tw)
            .unwrap_or(20.0);
        p.heading(
            &hero_info.title,
            tx - 1.0,
            sy + 28.0,
            tw,
            44.0,
            size,
            pal.text,
        );
        p.text(&hero_info.meta, tx, sy + 76.0, tw, 20.0, 13.0, pal.text2);
        if !hero_info.description.is_empty() {
            p.text(
                &hero_info.description,
                tx,
                sy + 98.0,
                tw,
                20.0,
                12.5,
                pal.text3,
            );
        }

        // Actions.
        let by = 4.0 + 188.0 - BUTTON_H;
        let mut bx = tx;
        let play_label = if plays_here { "暂停" } else { "播放全部" };
        bx += p.button(
            cx,
            rid(A_PLAY, 0),
            play_label,
            Some(if plays_here { g::PAUSE } else { g::PLAY }),
            bx,
            by,
            BUTTON_H,
            Btn::Primary,
            hover,
        ) + 10.0;
        // Secondary actions: labeled, or round glyph buttons when narrow.
        let secondary = |p: &mut P, bx: &mut f32, kind: u32, label: &str, glyph: char| {
            let id = rid(kind, 0);
            if compact {
                p.icon_button(
                    id,
                    glyph,
                    *bx + BUTTON_H / 2.0,
                    by + BUTTON_H / 2.0,
                    BUTTON_H,
                    12.5,
                    Some(if hover == Some(id) {
                        pal.pressed
                    } else {
                        pal.raised
                    }),
                    pal.text,
                );
                p.tip(id, label);
                *bx += BUTTON_H + 10.0;
            } else {
                *bx += p.button(
                    cx,
                    id,
                    label,
                    Some(glyph),
                    *bx,
                    by,
                    BUTTON_H,
                    Btn::Secondary,
                    hover,
                ) + 10.0;
            }
        };
        secondary(&mut p, &mut bx, A_SHUFFLE, "随机播放", g::SHUFFLE);
        if hero_info.save {
            secondary(&mut p, &mut bx, A_SAVE, "收藏为我的歌单", g::ADD);
        }
        if hero_info.add {
            secondary(&mut p, &mut bx, A_ADD, "添加音乐", g::ADD);
        }
        if hero_info.more {
            let id = rid(A_MORE, 0);
            p.icon_button(
                id,
                g::MORE,
                bx + BUTTON_H / 2.0,
                by + BUTTON_H / 2.0,
                BUTTON_H,
                13.0,
                Some(if hover == Some(id) {
                    pal.pressed
                } else {
                    pal.raised
                }),
                pal.text,
            );
            p.tip(id, "更多");
            bx += BUTTON_H + 10.0;
        }
        let _ = bx;
        p.finish()
    })
    .height(Dp::new(HERO_H));

    let table_view = TableView::new(
        state,
        rows,
        (info.empty.1, info.empty.2),
        info.load_more.clone(),
        info.show_tag,
    )
    .ranked(info.ranked)
    .empty_glyph(info.empty.0);
    let key = page.scroll_key();
    scroller(state, &key, column([hero, table(state, table_view)]))
}

pub struct TableView {
    pub rows: Rows,
    pub current: Option<String>,
    pub playing: bool,
    pub selected: Option<String>,
    pub loved: Arc<HashMap<String, String>>,
    pub more_label: Option<String>,
    pub empty: (String, String),
    pub empty_glyph: char,
    pub show_tag: bool,
    pub ranked: bool,
    pub covers: Covers,
}

impl TableView {
    pub fn new(
        state: &State,
        rows: Rows,
        empty: (&str, &str),
        more: Option<String>,
        show_tag: bool,
    ) -> Self {
        let covers = Covers::new(state);
        let store = state.bridge.store();
        TableView {
            rows,
            current: store.snapshot.now_playing.as_ref().map(|t| t.id.clone()),
            playing: super::now::is_active(store.snapshot.playback.status),
            selected: state.ui.selected.clone(),
            loved: store.love.clone(),
            more_label: more,
            empty: (empty.0.to_string(), empty.1.to_string()),
            empty_glyph: g::NOTE,
            show_tag,
            ranked: false,
            covers,
        }
    }

    pub fn ranked(mut self, ranked: bool) -> Self {
        self.ranked = ranked;
        self
    }

    pub fn empty_glyph(mut self, glyph: char) -> Self {
        self.empty_glyph = glyph;
        self
    }
}

/// The track table: a column header, then one row per track.
pub fn table(state: &State, view: TableView) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::List);
    let phase = crate::fmt::phase_ms();
    surface(Surface::List, move |cx: &ZsCanvasLayoutContext<'_>| {
        let _cover_frame = view.covers.begin_frame(cx);
        let mut p = P::new(pal);
        p.fill_visible(cx, pal.surface);
        let w = cx.size().width.0;
        let rows = &view.rows.items;
        let footer = if view.more_label.is_some() { 76.0 } else { 0.0 };
        p.scene.set_extent_height(Dp::new(
            TABLE_HEAD + rows.len() as f32 * ROW_H + footer + 40.0,
        ));

        // Columns.
        let x0 = PAD - 10.0;
        let x1 = w - PAD + 10.0;
        let index_w = if view.ranked { 48.0 } else { 40.0 };
        let tx = x0 + index_w + 8.0;
        let more_x = x1 - 38.0;
        let time_x = more_x - 54.0;
        let love_x = time_x - 38.0;
        let wide = w >= 620.0;
        let album_x = if wide {
            tx + (love_x - tx) * 0.58
        } else {
            love_x
        };
        let title_w = album_x - tx - 16.0;

        p.text_c("#", x0, 6.0, index_w, 28.0, 12.0, pal.text3);
        p.text("标题", tx, 6.0, 120.0, 28.0, 12.0, pal.text3);
        if wide {
            p.text("专辑", album_x, 6.0, 120.0, 28.0, 12.0, pal.text3);
        }
        p.glyph(g::TIMER, time_x + 28.0, 6.0, 20.0, 28.0, 10.5, pal.text3);
        p.hline(x0 + 10.0, TABLE_HEAD - 1.0, x1 - x0 - 20.0, pal.line);

        if rows.is_empty() {
            super::empty_state(
                &mut p,
                0.0,
                TABLE_HEAD + 40.0,
                w,
                view.empty_glyph,
                &view.empty.0,
                &view.empty.1,
            );
            p.scene.set_extent_height(Dp::new(TABLE_HEAD + 200.0));
            return p.finish();
        }
        let visible = cx.visible_rect();
        let top = visible.y.0 - TABLE_HEAD;
        let first = ((top / ROW_H).floor() as isize - 2).max(0) as usize;
        let last = (((top + visible.height.0) / ROW_H).ceil() as usize + 2).min(rows.len());
        let scale = cx.scale_factor();
        for index in first..last {
            let row = &rows[index];
            let y = TABLE_HEAD + index as f32 * ROW_H;
            let region = |kind| rid(kind, index as u32);
            let hot = hover.is_some_and(|h| {
                let (k, i) = rsplit(h);
                i == index as u32 && is_row_region(k)
            });
            let is_current = view.current.as_deref() == Some(row.id.as_str());
            let is_selected = view.selected.as_deref() == Some(row.id.as_str());
            if is_selected {
                p.round(x0, y + 2.0, x1 - x0, ROW_H - 4.0, 8.0, pal.pressed);
            } else if hot {
                p.round(x0, y + 2.0, x1 - x0, ROW_H - 4.0, 8.0, pal.hover);
            }
            p.hover_plain(region(R_ROW), x0, y, x1 - x0, ROW_H);

            // Index; play on hover; level bars while current.
            if hot {
                let glyph = if is_current && view.playing {
                    g::PAUSE
                } else {
                    g::PLAY
                };
                p.glyph(glyph, x0, y, index_w, ROW_H, 11.5, pal.text);
                p.hover(region(R_PLAY), x0, y + 8.0, index_w, ROW_H - 16.0);
                p.name(region(R_PLAY), "播放");
            } else if is_current {
                bars(
                    &mut p,
                    x0 + index_w / 2.0 - 6.0,
                    y + ROW_H / 2.0,
                    view.playing,
                    phase,
                    pal.text,
                );
            } else if view.ranked {
                let top3 = row.index < 3;
                p.num(
                    &(row.index + 1).to_string(),
                    x0,
                    y,
                    index_w,
                    ROW_H,
                    if top3 { 19.0 } else { 15.0 },
                    if top3 { pal.text } else { pal.text3 },
                    HorizontalAlign::Center,
                    if top3 {
                        TextWeight::Bold
                    } else {
                        TextWeight::Regular
                    },
                );
            } else {
                p.num(
                    &(index + 1).to_string(),
                    x0,
                    y,
                    index_w,
                    ROW_H,
                    13.0,
                    pal.text3,
                    HorizontalAlign::Center,
                    TextWeight::Regular,
                );
            }

            let art = view.covers.row(row, &view.rows.source);
            covers::draw(
                &mut p,
                art.as_deref(),
                tx,
                y + 8.0,
                40.0,
                6.0,
                scale,
                &row.title,
            );
            let lx = tx + 52.0;
            let lw = title_w - 52.0;
            p.text_ex(
                &row.title,
                lx,
                y + 8.0,
                lw,
                22.0,
                14.0,
                pal.text,
                HorizontalAlign::Start,
                if is_current {
                    TextWeight::Bold
                } else {
                    TextWeight::Regular
                },
            );
            let mut sx = lx;
            if view.show_tag {
                let tag = source_tag(&row.source);
                if !tag.is_empty() {
                    let tag_w = measure(cx, tag, 10.0, TextWeight::Regular) + 10.0;
                    p.round(sx, y + 32.0, tag_w, 15.0, 4.0, pal.raised);
                    p.text_c(tag, sx, y + 32.0, tag_w, 15.0, 10.0, pal.text3);
                    sx += tag_w + 6.0;
                }
            }
            let artist = if row.artist.is_empty() {
                "未知歌手"
            } else {
                row.artist.as_str()
            };
            p.text(artist, sx, y + 30.0, lw - (sx - lx), 19.0, 12.5, pal.text2);
            if wide {
                p.text(
                    &row.album,
                    album_x,
                    y,
                    love_x - album_x - 12.0,
                    ROW_H,
                    13.0,
                    pal.text2,
                );
            }

            let loved = view.loved.contains_key(&row.love_key);
            if loved || hot {
                let love_hot = hover == Some(region(R_LOVE));
                let (glyph, color) = if loved {
                    (g::HEART_FILL, pal.love)
                } else {
                    (g::HEART, if love_hot { pal.text } else { pal.text3 })
                };
                p.glyph(glyph, love_x, y, 32.0, ROW_H, 12.5, color);
                p.hover(region(R_LOVE), love_x, y + 12.0, 32.0, ROW_H - 24.0);
                p.tip(region(R_LOVE), if loved { "取消喜欢" } else { "喜欢" });
            }
            p.text_family(
                &clock(row.seconds),
                time_x,
                y,
                48.0,
                ROW_H,
                12.5,
                pal.text3,
                HorizontalAlign::End,
                TextWeight::Regular,
                Some(NUM_FONT),
            );
            if hot {
                let more_hot = hover == Some(region(R_MORE));
                p.glyph(
                    g::MORE,
                    more_x,
                    y,
                    28.0,
                    ROW_H,
                    12.5,
                    if more_hot { pal.text } else { pal.text3 },
                );
                p.hover(region(R_MORE), more_x, y + 12.0, 28.0, ROW_H - 24.0);
                p.tip(region(R_MORE), "更多");
            }
        }
        if let Some(label) = &view.more_label {
            let y = TABLE_HEAD + rows.len() as f32 * ROW_H + 20.0;
            let bw = measure(cx, label, 12.5, TextWeight::Semibold) + 28.0;
            let x = (w - bw) / 2.0;
            p.button(
                cx,
                rid(R_MORE_PAGE, 0),
                label,
                None,
                x,
                y,
                34.0,
                Btn::Secondary,
                hover,
            );
        }
        p.finish()
    })
}

pub fn click(state: &mut State, region: u64, x: f32, y: f32) {
    if shared_click(state, region) {
        return;
    }
    let page = state.ui.page.clone();
    match rsplit(region).0 {
        A_PLAY => {
            let (origin, playing) = {
                let store = state.bridge.store();
                (
                    store.queue.as_ref().map(|q| q.origin_list.clone()),
                    super::now::is_active(store.snapshot.playback.status),
                )
            };
            let own = match &page {
                Page::Songs => Some("catalog".to_string()),
                Page::List(id) => Some(id.clone()),
                _ => None,
            };
            if playing && own.is_some() && own == origin {
                app::act_command(state, BackendCommand::Pause);
            } else {
                state.play_collection(&page);
            }
        }
        A_SHUFFLE => app::act_shuffle_page(state),
        A_SAVE => app::act_save_page(state),
        A_ADD => app::act_open_menu(state, MenuKind::Create, Surface::Hero, x, y + 20.0),
        A_MORE => {
            let kind = match &page {
                Page::Songs => MenuKind::Collection(Page::Songs),
                Page::List(_) | Page::Playlist(..) | Page::Charts => {
                    MenuKind::Collection(page.clone())
                }
                _ => return,
            };
            app::act_open_menu(state, kind, Surface::Hero, x, y + 20.0);
        }
        _ => {}
    }
}

pub fn table_click(state: &mut State, region: u64, double: bool, x: f32, y: f32) {
    let (kind, index) = rsplit(region);
    if kind == R_MORE_PAGE {
        load_more(state);
        return;
    }
    let rows = state.rows.rows(state, &state.ui.page);
    let Some(row) = rows.items.get(index as usize).cloned() else {
        return;
    };
    match kind {
        R_ROW => {
            state.ui.selected = Some(row.id.clone());
            if double {
                state.play_row(&row, &rows.source);
            }
        }
        R_PLAY => {
            let (current, playing) = {
                let store = state.bridge.store();
                (
                    store.snapshot.now_playing.as_ref().map(|t| t.id.clone()),
                    super::now::is_active(store.snapshot.playback.status),
                )
            };
            if current.as_deref() == Some(row.id.as_str()) {
                let command = if playing {
                    BackendCommand::Pause
                } else {
                    BackendCommand::Resume
                };
                app::act_command(state, command);
            } else {
                state.play_row(&row, &rows.source);
            }
        }
        R_LOVE => state.toggle_love(&row, &rows.source),
        R_MORE => {
            state.ui.selected = Some(row.id.clone());
            state.ui.menu = Some(Menu {
                kind: MenuKind::Track {
                    row,
                    context: rows.source.clone(),
                },
                target: Surface::List,
                x,
                y,
            });
        }
        _ => {}
    }
}

/// Loads the next page of search results, the open chart or playlist.
pub fn load_more(state: &mut State) {
    let store = state.bridge.store();
    let job = match &state.ui.page {
        Page::Search if store.search.load != Load::Running => {
            store.search.platform.map(|platform| Job::Search {
                platform,
                keyword: store.search.keyword.clone(),
                page: store.search.page + 1,
            })
        }
        Page::Charts if store.chart.load != Load::Running => {
            store.chart.platform.map(|platform| Job::ChartTracks {
                platform,
                id: store.chart.id.clone(),
                page: (store.chart.tracks.len() as u32 / CHART_PAGE) + 1,
            })
        }
        Page::Playlist(platform, id) if store.playlist.load != Load::Running => {
            Some(Job::PlaylistDetail {
                platform: *platform,
                id: id.clone(),
                page: (store.playlist.tracks.len() as u32 / PLAYLIST_TRACKS) + 1,
            })
        }
        _ => None,
    };
    drop(store);
    if let Some(job) = job {
        app::act_job(state, job);
    }
}

fn item(label: &str, id: &str) -> MenuItemSpec {
    MenuItemSpec::command(label, Command::custom(id))
}

fn import_items(mut spec: MenuSpec) -> MenuSpec {
    spec.items
        .push(item("通过链接导入歌单…", "import_playlist_url"));
    spec.items
        .push(item("导入歌单文件…", "import_playlist_file"));
    spec = spec.separator();
    spec.items.push(item("导入音频文件…", "import_files"));
    spec.items.push(item("添加音乐文件夹…", "add_folder"));
    spec = spec.separator();
    spec.items.push(item("重新扫描所有文件夹", "rescan"));
    spec
}

/// "Add to playlist" entries for every playlist of the user.
fn list_targets(state: &State, current: Option<&str>) -> MenuSpec {
    let mut lists = MenuSpec::new();
    let store = state.bridge.store();
    for list in store
        .lists
        .iter()
        .filter(|l| l.id != "temp" && l.id != "love")
    {
        let mut entry = item(&list_display_name(list), &format!("add:{}", list.id));
        if current == Some(list.id.as_str()) {
            entry = entry.disabled();
        }
        lists.items.push(entry);
    }
    lists
}

fn timer_items(mut spec: MenuSpec) -> MenuSpec {
    for minutes in [15, 30, 45, 60, 90] {
        spec.items.push(item(
            &format!("{minutes} 分钟后"),
            &format!("timer:{minutes}"),
        ));
    }
    spec = spec.separator();
    spec.items.push(item("取消定时", "timer:cancel"));
    spec
}

pub fn menu_spec(state: &State, menu: &Menu) -> MenuSpec {
    let mut spec = MenuSpec::new();
    match &menu.kind {
        MenuKind::Track { row, context } => {
            let loved = state.bridge.store().love.contains_key(&row.love_key);
            spec.items.push(item("播放", "play"));
            spec.items.push(item("下一首播放", "next"));
            spec = spec.separator();
            spec.items.push(item(
                if loved {
                    "从我喜欢中移除"
                } else {
                    "添加到我喜欢"
                },
                "love",
            ));
            let current = match context {
                RowSource::List(id) => Some(id.as_str()),
                _ => None,
            };
            spec = spec.submenu("添加到歌单", list_targets(state, current));
            spec = spec.separator();
            if row.source == "local" {
                if !row.album.is_empty() {
                    spec.items.push(item("查看专辑", "album"));
                }
                if !row.artist.is_empty() {
                    spec.items.push(item("查看歌手", "artist"));
                }
            } else if !row.artist.is_empty() {
                spec.items.push(item("搜索这位歌手", "search_artist"));
            }
            if matches!(context, RowSource::List(id) if id != "temp") {
                spec = spec.separator();
                spec.items.push(item("从歌单中移除", "remove"));
            }
        }
        MenuKind::List(id) | MenuKind::Collection(Page::List(id)) => {
            if matches!(menu.kind, MenuKind::List(_)) {
                spec.items.push(item("打开", "open"));
                spec.items.push(item("播放全部", "play"));
                spec = spec.separator();
            }
            if matches!(id.as_str(), "default" | "love" | "temp") {
                spec.items.push(item("清空歌单", "clear"));
            } else {
                spec.items.push(item("重命名", "rename"));
                spec.items.push(item("删除歌单", "delete"));
            }
        }
        MenuKind::Collection(Page::Playlist(..)) | MenuKind::Collection(Page::Charts) => {
            spec.items.push(item("收藏为我的歌单", "save"));
        }
        MenuKind::Create => {
            spec.items.push(item("新建歌单…", "new_list"));
            spec = spec.separator();
            spec = import_items(spec);
        }
        MenuKind::Collection(_) => spec = import_items(spec),
        MenuKind::NowPlaying => {
            let loved = super::now::now(state).loved;
            spec.items.push(item(
                if loved {
                    "从我喜欢中移除"
                } else {
                    "添加到我喜欢"
                },
                "love",
            ));
            spec = spec.submenu("添加到歌单", list_targets(state, None));
            spec = spec.separator();
            spec.items.push(item("歌词提前 0.5 秒", "lyrics_earlier"));
            spec.items.push(item("歌词延后 0.5 秒", "lyrics_later"));
            spec.items.push(item("歌词时间复位", "lyrics_reset"));
            spec = spec.separator();
            spec = spec.submenu("定时停止", timer_items(MenuSpec::new()));
            spec.items.push(item("音效与输出设置", "effects"));
            spec.items.push(item("全屏播放", "stage"));
        }
        MenuKind::Timer => spec = timer_items(spec),
        MenuKind::Tags => {
            let store = state.bridge.store();
            let tags = store
                .tag_list
                .get(&state.ui.browse)
                .cloned()
                .unwrap_or_default();
            drop(store);
            spec.items.push(item("推荐", "tag:"));
            let mut groups: Vec<String> = Vec::new();
            for tag in &tags {
                if !groups.contains(&tag.group) {
                    groups.push(tag.group.clone());
                }
            }
            for group in groups {
                let mut sub = MenuSpec::new();
                for tag in tags.iter().filter(|t| t.group == group) {
                    sub.items.push(item(&tag.name, &format!("tag:{}", tag.id)));
                }
                let title = if group.trim().is_empty() {
                    "其他".to_string()
                } else {
                    group
                };
                spec = spec.submenu(&title, sub);
            }
        }
    }
    let _ = with_alpha;
    let _: Option<Platform> = None;
    spec
}
