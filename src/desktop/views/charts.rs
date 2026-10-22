//! 排行榜: every chart of the browsing platform in a column on the left,
//! the open chart as a collection on the right.

use super::{
    PAD,
    covers::{self, Covers},
    failure, scroller, shared_click, skeleton, surface,
};
use crate::{
    app::{self, PLATFORM_NAMES, PLATFORMS, Page, State, Surface},
    bridge::Load,
    paint::{P, rid, rsplit},
    theme::Palette,
};
use zsui::{Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext, row};

use crate::app::Msg;

pub const INDEX_W: f32 = 232.0;
const C_PLATFORM: u32 = 1;
const C_CHART: u32 = 2;
const ITEM_H: f32 = 56.0;

pub fn view(state: &State) -> ViewNode<Msg> {
    row([
        index(state).width(Dp::new(INDEX_W)),
        super::collection::view(state).flex(1.0),
    ])
}

fn index(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::ChartIndex);
    let browse = state.ui.browse;
    let covers = Covers::new(state);
    let (charts, load, open) = {
        let store = state.bridge.store();
        (
            store.chart_list.get(&browse).cloned().unwrap_or_default(),
            store.charts.get(&browse).cloned().unwrap_or(Load::Running),
            (store.chart.platform == Some(browse)).then(|| store.chart.id.clone()),
        )
    };
    let content = surface(
        Surface::ChartIndex,
        move |cx: &ZsCanvasLayoutContext<'_>| {
            let _cover_frame = covers.begin_frame(cx);
            let mut p = P::new(pal);
            p.fill_visible(cx, pal.surface);
            let w = cx.size().width.0;
            let scale = cx.scale_factor();
            let x0 = PAD;
            let iw = w - x0 - 12.0;
            let mut y = 0.0;
            super::page_title(&mut p, x0, y, iw, "排行榜");
            y += 58.0;
            // Platforms as chips, two lines.
            let mut x = x0;
            for (i, name) in PLATFORM_NAMES.iter().enumerate() {
                let cw = crate::paint::measure(cx, name, 12.5, TextWeight::Regular) + 26.0;
                if x + cw > x0 + iw {
                    x = x0;
                    y += 38.0;
                }
                p.chip(
                    cx,
                    rid(C_PLATFORM, i as u32),
                    name,
                    x,
                    y,
                    PLATFORMS[i] == browse,
                    hover,
                );
                x += cw + 6.0;
            }
            y += 50.0;
            match (&load, charts.is_empty()) {
                (Load::Failed(error), _) => {
                    failure(&mut p, cx, x0, y, iw, error, hover);
                    y += 48.0;
                }
                (_, true) => {
                    for i in 0..8 {
                        skeleton(&mut p, x0, y + i as f32 * ITEM_H + 8.0, 40.0, 40.0, 6.0);
                        skeleton(
                            &mut p,
                            x0 + 52.0,
                            y + i as f32 * ITEM_H + 22.0,
                            iw - 80.0,
                            12.0,
                            6.0,
                        );
                    }
                    y += 8.0 * ITEM_H;
                }
                _ => {
                    for (i, chart) in charts.iter().enumerate() {
                        if !covers::visible(cx, y, ITEM_H) {
                            y += ITEM_H;
                            continue;
                        }
                        let region = rid(C_CHART, i as u32);
                        let on = open.as_deref() == Some(chart.id.as_str());
                        let hot = hover == Some(region);
                        if on {
                            p.round(x0 - 8.0, y + 2.0, iw + 8.0, ITEM_H - 4.0, 8.0, pal.pressed);
                        } else if hot {
                            p.round(x0 - 8.0, y + 2.0, iw + 8.0, ITEM_H - 4.0, 8.0, pal.hover);
                        }
                        covers::draw(
                            &mut p,
                            covers.url(chart.image_url.as_deref()).as_deref(),
                            x0,
                            y + 8.0,
                            40.0,
                            6.0,
                            scale,
                            &chart.name,
                        );
                        p.text_ex(
                            &chart.name,
                            x0 + 52.0,
                            y,
                            iw - 60.0,
                            ITEM_H,
                            13.5,
                            if on || hot { pal.text } else { pal.text2 },
                            HorizontalAlign::Start,
                            if on {
                                TextWeight::Bold
                            } else {
                                TextWeight::Regular
                            },
                        );
                        p.hover(region, x0 - 8.0, y, iw + 8.0, ITEM_H);
                        y += ITEM_H;
                    }
                }
            }
            p.scene.set_extent_height(Dp::new(y + 24.0));
            p.finish()
        },
    );
    scroller(state, "chart-index", content)
}

pub fn click(state: &mut State, region: u64) {
    if shared_click(state, region) {
        return;
    }
    let (kind, index) = rsplit(region);
    match kind {
        C_PLATFORM => {
            if let Some(platform) = PLATFORMS.get(index as usize).copied() {
                app::act_set_browse(state, platform);
            }
        }
        C_CHART => app::act_open_chart(state, index as usize),
        _ => {}
    }
    let _ = Page::Charts;
}
