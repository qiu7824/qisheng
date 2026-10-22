//! Custom sources: import, approve, enable and remove LX source scripts.

use super::{PAD, scroller, surface};
use crate::{
    app::{self, Modal, Msg, State, Surface},
    bridge::Job,
    paint::{P, measure, rid, rsplit},
    theme::{Palette, glyph as g, with_alpha},
};
use lx_gpui_player::backend::BackendCommand;
use zsui::{Color, Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext};

const S_APPROVE: u32 = 70;
const S_ENABLE: u32 = 71;
const S_DISABLE: u32 = 72;
const S_DELETE: u32 = 73;
const S_IMPORT: u32 = 74;
const S_DEMO: u32 = 75;

pub fn view(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Sources);
    let list = state.bridge.store().snapshot.sources.clone();
    let subtitle = match list.iter().find(|s| s.enabled) {
        Some(source) => format!("正在使用「{}」", source.name),
        None => "未启用音源".into(),
    };
    let canvas = surface(Surface::Sources, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        p.fill_visible(cx, pal.surface);
        let w = cx.size().width.0;
        let x0 = PAD;
        let cw = (w - PAD * 2.0).min(880.0);
        super::page_title(&mut p, x0, 0.0, cw, "音源");
        p.text(&subtitle, x0, 46.0, cw, 20.0, 12.5, pal.text3);
        let mut bx = x0;
        for (kind, label, primary) in [
            (S_IMPORT, "导入音源脚本", true),
            (S_DEMO, "添加演示音源", false),
        ] {
            let region = rid(kind, 0);
            let bw = measure(cx, label, 13.0, TextWeight::Semibold) + 36.0;
            let hot = hover == Some(region);
            let (fill, fg) = if primary {
                (
                    if hot {
                        with_alpha(pal.control, 0xe0)
                    } else {
                        pal.control
                    },
                    pal.on_control,
                )
            } else {
                (if hot { pal.hover } else { pal.surface }, pal.text)
            };
            p.round_stroke(
                bx,
                80.0,
                bw,
                36.0,
                18.0,
                fill,
                if primary { fill } else { pal.line_strong },
            );
            p.text_ex(
                label,
                bx,
                80.0,
                bw,
                36.0,
                13.0,
                fg,
                HorizontalAlign::Center,
                TextWeight::Semibold,
            );
            p.hover(region, bx, 80.0, bw, 36.0);
            bx += bw + 10.0;
        }
        let mut y = 140.0;
        // How sources are trusted, in one quiet panel.
        p.round(x0, y, cw, 66.0, 14.0, pal.raised);
        p.glyph(g::SHIELD, x0 + 14.0, y, 28.0, 66.0, 18.0, pal.text2);
        p.text(
            "音源是第三方脚本，用来获取在线歌曲的播放地址。",
            x0 + 54.0,
            y + 11.0,
            cw - 70.0,
            22.0,
            13.0,
            pal.text,
        );
        p.text(
            "批准并启用后才会在沙箱中运行；脚本改动后需要重新批准。",
            x0 + 54.0,
            y + 34.0,
            cw - 70.0,
            22.0,
            12.0,
            pal.text3,
        );
        y += 86.0;
        if list.is_empty() {
            p.glyph(g::SOURCE, x0, y + 24.0, cw, 48.0, 32.0, pal.line_strong);
            p.text_ex(
                "还没有音源",
                x0,
                y + 80.0,
                cw,
                26.0,
                15.0,
                pal.text2,
                HorizontalAlign::Center,
                TextWeight::Semibold,
            );
            p.text_c(
                "导入 LX Music 兼容的音源脚本，或者先添加演示音源试试",
                x0,
                y + 108.0,
                cw,
                22.0,
                12.5,
                pal.text3,
            );
            y += 160.0;
        }
        for (i, source) in list.iter().enumerate() {
            let h = 100.0;
            let border = if source.enabled { pal.text2 } else { pal.line };
            p.round_stroke(x0, y, cw, h, 14.0, pal.surface, border);
            let (badge, badge_color): (&str, Color) = if source.enabled {
                ("使用中", pal.ok)
            } else if source.approved {
                ("已批准", pal.text2)
            } else {
                ("待批准", pal.warn)
            };
            let title = if source.name.is_empty() {
                "未命名音源"
            } else {
                source.name.as_str()
            };
            let tw = measure(cx, title, 16.0, TextWeight::Semibold).min(cw - 300.0);
            p.text_ex(
                title,
                x0 + 20.0,
                y + 14.0,
                tw + 4.0,
                26.0,
                16.0,
                pal.text,
                HorizontalAlign::Start,
                TextWeight::Semibold,
            );
            let bx = x0 + 20.0 + tw + 12.0;
            p.round(
                bx,
                y + 18.0,
                54.0,
                20.0,
                10.0,
                with_alpha(badge_color, 0x30),
            );
            p.text_c(badge, bx, y + 18.0, 54.0, 20.0, 11.0, badge_color);
            let meta = format!(
                "版本 {} · 作者 {} · SHA-256 {}…",
                if source.version.is_empty() {
                    "-"
                } else {
                    &source.version
                },
                if source.author.is_empty() {
                    "未知"
                } else {
                    &source.author
                },
                source.hash.chars().take(12).collect::<String>()
            );
            p.text(
                &meta,
                x0 + 20.0,
                y + 42.0,
                cw - 260.0,
                20.0,
                12.0,
                pal.text3,
            );
            let desc = if source.description.is_empty() {
                source.status.clone()
            } else {
                format!("{}  ·  {}", source.description, source.status)
            };
            p.text(&desc, x0 + 20.0, y + 70.0, cw - 40.0, 22.0, 12.5, pal.text2);
            let actions: [(&str, u32, bool); 2] = if source.enabled {
                [("删除", S_DELETE, false), ("停用", S_DISABLE, false)]
            } else if source.approved {
                [("删除", S_DELETE, false), ("启用", S_ENABLE, true)]
            } else {
                [("删除", S_DELETE, false), ("批准", S_APPROVE, true)]
            };
            let mut ax = x0 + cw - 20.0;
            for (label, kind, primary) in actions {
                let region = rid(kind, i as u32);
                let bw = 66.0;
                ax -= bw;
                let hot = hover == Some(region);
                let (fill, fg) = if primary {
                    (
                        if hot {
                            with_alpha(pal.control, 0xe0)
                        } else {
                            pal.control
                        },
                        pal.on_control,
                    )
                } else if kind == S_DELETE && hot {
                    (with_alpha(pal.danger, 0x30), pal.danger)
                } else {
                    (if hot { pal.pressed } else { pal.raised }, pal.text)
                };
                p.round(ax, y + 14.0, bw, 32.0, 16.0, fill);
                p.text_ex(
                    label,
                    ax,
                    y + 14.0,
                    bw,
                    32.0,
                    12.5,
                    fg,
                    HorizontalAlign::Center,
                    TextWeight::Semibold,
                );
                p.hover(region, ax, y + 14.0, bw, 32.0);
                ax -= 8.0;
            }
            y += h + 14.0;
        }
        p.scene.set_extent_height(Dp::new(y + 16.0));
        p.finish()
    });
    scroller(state, "sources", canvas)
}

pub fn click(state: &mut State, region: u64) {
    let (kind, index) = rsplit(region);
    match kind {
        S_IMPORT => return app::act_import_source(state),
        S_DEMO => {
            return app::act_job(
                state,
                Job::CommandNotice(BackendCommand::AddDemoSource, "已添加演示音源".into()),
            );
        }
        _ => {}
    }
    let source = state
        .bridge
        .store()
        .snapshot
        .sources
        .get(index as usize)
        .cloned();
    let Some(source) = source else { return };
    match kind {
        S_APPROVE => app::act_job(
            state,
            Job::CommandNotice(
                BackendCommand::ApproveSource { id: source.id },
                "已批准，点击「启用」开始使用".into(),
            ),
        ),
        S_ENABLE => app::act_job(
            state,
            Job::CommandNotice(
                BackendCommand::EnableSource { id: source.id },
                "音源已启用".into(),
            ),
        ),
        S_DISABLE => app::act_job(
            state,
            Job::CommandNotice(BackendCommand::DisableSource, "音源已停用".into()),
        ),
        S_DELETE => {
            state.ui.modal = Some(Modal::DeleteSource {
                id: source.id,
                name: source.name,
            })
        }
        _ => {}
    }
}
