//! Layers above the main window content: toasts, the playback-address
//! approval card and modal dialogs. They are stacked with overlapping Grid
//! cells; the toast layer is a non-interactive canvas so pointer input
//! passes through it.

use super::{NAV_W, PANEL_W, surface};
use crate::{
    app::{Modal, Msg, State, Surface, W_MODAL_INPUT},
    bridge::{Job, ToastKind},
    paint::{P, para_style, rid, rsplit},
    theme::{Palette, glyph as g, with_alpha},
};
use lx_gpui_player::backend::BackendCommand;
use zsui::{
    Dp, HorizontalAlign, TextWeight, ViewNode, ZsCanvasLayoutContext, ZsGridCell, ZsGridTrack,
    canvas_with, column, grid, row, spacer, textbox,
};

pub const R_OK: u32 = 120;
pub const R_CANCEL: u32 = 121;
const R_ALWAYS: u32 = 122;
const R_IMPORT_PLATFORM: u32 = 123;

const DIALOG_W: f32 = 460.0;

/// What the modal layer shows, in priority order.
enum Layer {
    Importing,
    ImportPreview {
        lists: usize,
        local_references: usize,
    },
    Approval(String),
    Modal(Modal),
}

pub fn layer(state: &State, body: ViewNode<Msg>) -> ViewNode<Msg> {
    let pending = state.bridge.store().snapshot.pending_playback_url.clone();
    let (preview, importing) = {
        let store = state.bridge.store();
        (store.playlist_preview.clone(), store.import_busy)
    };
    let layer = match (&state.ui.modal, pending) {
        (Some(modal), _) => Some(Layer::Modal(modal.clone())),
        (None, _) if preview.is_some() => {
            let preview = preview.unwrap();
            Some(Layer::ImportPreview {
                lists: preview.imported_lists,
                local_references: preview.local_references,
            })
        }
        (None, _) if importing => Some(Layer::Importing),
        (None, Some(url)) => Some(Layer::Approval(url)),
        (None, None) => None,
    };
    let mut cells = vec![
        ZsGridCell::new(0, 0, body),
        ZsGridCell::new(0, 0, toasts(state)),
    ];
    if let Some(layer) = layer {
        cells.push(ZsGridCell::new(0, 0, scrim()));
        cells.push(ZsGridCell::new(0, 0, dialog(state, layer)));
    }
    grid([ZsGridTrack::FLEX], [ZsGridTrack::FLEX], cells)
}

fn toasts(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let toasts: Vec<(String, ToastKind)> = state
        .bridge
        .store()
        .toasts
        .iter()
        .map(|t| (t.text.clone(), t.kind))
        .collect();
    let stage = state.ui.stage;
    // No id and no handlers: the canvas never takes pointer input.
    canvas_with(move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let size = cx.size();
        // Over the page column, or the middle of the stage.
        let (left, shelf_w) = if stage {
            (0.0, size.width.0.max(200.0))
        } else {
            (NAV_W, (size.width.0 - NAV_W - PANEL_W).max(200.0))
        };
        let mut y = size.height.0 - if stage { 150.0 } else { 28.0 };
        for (text, kind) in toasts.iter().rev() {
            let tw =
                crate::paint::measure(cx, text, 13.0, TextWeight::Regular).min(shelf_w - 120.0);
            let w = tw + 64.0;
            let h = 40.0;
            y -= h;
            let x = left + (shelf_w - w) / 2.0;
            let (glyph, color) = match kind {
                ToastKind::Success => (g::CHECK, pal.ok),
                ToastKind::Error => (g::WARNING, pal.danger),
                ToastKind::Info => (g::INFO, pal.on_control),
            };
            p.shadow_ex(x, y, w, h, h / 2.0, 0x60, 6.0, 12.0);
            // Inverted, like a system snackbar.
            p.round(x, y, w, h, h / 2.0, pal.control);
            p.glyph(glyph, x + 14.0, y, 20.0, h, 13.0, color);
            p.text(text, x + 42.0, y, tw + 4.0, h, 13.0, pal.on_control);
            y -= 10.0;
        }
        p.finish()
    })
}

fn scrim() -> ViewNode<Msg> {
    let pal = Palette::get();
    surface(Surface::Scrim, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let size = cx.size();
        p.fill(0.0, 0.0, size.width.0, size.height.0, pal.scrim);
        p.hover_plain(rid(1, 0), 0.0, 0.0, size.width.0, size.height.0);
        p.finish()
    })
}

struct DialogText {
    title: String,
    message: String,
    ok: String,
    cancel: String,
    danger: bool,
    always: Option<bool>,
}

fn dialog(state: &State, layer: Layer) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Modal);
    let platform = match &layer {
        Layer::Modal(Modal::ImportPlaylist { platform, .. }) => Some(*platform),
        _ => None,
    };
    let placeholder = match &layer {
        Layer::Modal(Modal::ImportPlaylist { .. }) => "歌单链接或 ID",
        Layer::Modal(Modal::ApiPort { .. }) => "23330",
        _ => "歌单名称",
    };
    let can_confirm = !matches!(layer, Layer::Importing);
    let (text, input) = match &layer {
        Layer::Importing => (
            DialogText {
                title: "正在读取歌单".into(),
                message: "歌单会在完整读取后导入，可随时取消。".into(),
                ok: "读取中…".into(),
                cancel: "取消".into(),
                danger: false,
                always: None,
            },
            None,
        ),
        Layer::ImportPreview {
            lists,
            local_references,
        } => (
            DialogText {
                title: "确认导入歌单".into(),
                message: format!(
                    "文件包含 {lists} 个歌单。同一歌单 ID 的歌曲将被替换，其他歌单保留。\n本地音乐引用：{local_references}；本地文件需另行添加到音乐库。"
                ),
                ok: "导入".into(),
                cancel: "取消".into(),
                danger: false,
                always: None,
            },
            None,
        ),
        Layer::Modal(Modal::ImportPlaylist { input, .. }) => (
            DialogText {
                title: "通过链接导入歌单".into(),
                message: "选择歌单平台，粘贴完整分享链接或歌单 ID。".into(),
                ok: "导入".into(),
                cancel: "取消".into(),
                danger: false,
                always: None,
            },
            Some(input.clone()),
        ),
        Layer::Modal(Modal::ApiPort { input }) => (
            DialogText {
                title: "设置接口端口".into(),
                message: "输入 1–65535；修改后重新启动已启用的接口。".into(),
                ok: "保存".into(),
                cancel: "取消".into(),
                danger: false,
                always: None,
            },
            Some(input.clone()),
        ),
        Layer::Approval(url) => (
            DialogText {
                title: "允许播放这个地址？".into(),
                message: format!(
                    "音源返回了下面的网络地址。允许后播放器才会开始下载并播放：\n{}",
                    shorten(url, 180)
                ),
                ok: "允许并播放".into(),
                cancel: "取消".into(),
                danger: false,
                always: Some(state.bridge.store().auto_approve_audio),
            },
            None,
        ),
        Layer::Modal(Modal::NewList { name }) => (
            DialogText {
                title: "新建歌单".into(),
                message: "给歌单起个名字".into(),
                ok: "创建".into(),
                cancel: "取消".into(),
                danger: false,
                always: None,
            },
            Some(name.clone()),
        ),
        Layer::Modal(Modal::RenameList { name, .. }) => (
            DialogText {
                title: "重命名歌单".into(),
                message: "输入新的歌单名称".into(),
                ok: "保存".into(),
                cancel: "取消".into(),
                danger: false,
                always: None,
            },
            Some(name.clone()),
        ),
        Layer::Modal(Modal::DeleteList { name, .. }) => (
            DialogText {
                title: format!("删除歌单「{name}」？"),
                message: "歌单里的歌曲记录会一起删除，本地文件不受影响。此操作无法撤销。".into(),
                ok: "删除".into(),
                cancel: "取消".into(),
                danger: true,
                always: None,
            },
            None,
        ),
        Layer::Modal(Modal::DeleteSource { name, .. }) => (
            DialogText {
                title: format!("删除音源「{name}」？"),
                message: "删除后需要重新导入并批准才能再次使用。".into(),
                ok: "删除".into(),
                cancel: "取消".into(),
                danger: true,
                always: None,
            },
            None,
        ),
    };
    let body_style = para_style(13.0, HorizontalAlign::Start, TextWeight::Regular);
    let message = text.message.clone();
    let title = text.title.clone();
    let top = canvas_with(move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let w = cx.size().width.0;
        let measured = cx
            .measure_text_at(
                &message,
                body_style,
                Some(Dp::new(13.0)),
                Some(Dp::new(w - 56.0)),
            )
            .height
            .0;
        let h = 24.0 + 32.0 + 8.0 + measured + 16.0;
        p.scene.set_extent_height(Dp::new(h));
        p.text_ex(
            &title,
            28.0,
            24.0,
            w - 56.0,
            32.0,
            18.0,
            pal.text,
            HorizontalAlign::Start,
            TextWeight::Semibold,
        );
        p.para(
            &message,
            28.0,
            64.0,
            w - 56.0,
            measured + 4.0,
            13.0,
            body_style,
            pal.text2,
        );
        p.finish()
    });
    let mut parts = vec![top];
    if let Some(platform) = platform {
        parts.push(
            surface(Surface::Modal, move |cx: &ZsCanvasLayoutContext<'_>| {
                let mut p = P::new(pal);
                for (i, name) in crate::app::PLATFORM_NAMES.iter().enumerate() {
                    p.chip(
                        cx,
                        rid(R_IMPORT_PLATFORM, i as u32),
                        name,
                        28.0 + i as f32 * 76.0,
                        4.0,
                        crate::app::PLATFORMS[i] == platform,
                        hover,
                    );
                }
                p.finish()
            })
            .height(Dp::new(44.0)),
        );
    }
    if let Some(value) = input {
        parts.push(
            row([
                zsui::spacer().width(Dp::new(28.0)),
                textbox(value)
                    .id(W_MODAL_INPUT)
                    .placeholder(placeholder)
                    .on_change(Msg::ModalText)
                    .on_submit(Msg::ModalSubmit)
                    .flex(1.0),
                zsui::spacer().width(Dp::new(28.0)),
            ])
            .height(Dp::new(44.0)),
        );
    }
    let ok = text.ok.clone();
    let cancel = text.cancel.clone();
    let danger = text.danger;
    let always = text.always;
    parts.push(
        surface(Surface::Modal, move |cx: &ZsCanvasLayoutContext<'_>| {
            let mut p = P::new(pal);
            let w = cx.size().width.0;
            let h = 72.0;
            let _ = h;
            let bw = 108.0;
            let by = 20.0;
            let ok_x = w - 28.0 - bw;
            let cancel_x = ok_x - 10.0 - bw;
            let ok_hot = hover == Some(rid(R_OK, 0));
            let ok_fill = match (danger, ok_hot) {
                (true, false) => pal.danger,
                (true, true) => with_alpha(pal.danger, 0xd0),
                (false, false) => pal.control,
                (false, true) => with_alpha(pal.control, 0xe0),
            };
            p.pill(
                &ok,
                ok_x,
                by,
                bw,
                36.0,
                ok_fill,
                if danger { pal.white } else { pal.on_control },
                13.0,
            );
            if can_confirm {
                p.hover(rid(R_OK, 0), ok_x, by, bw, 36.0);
            }
            let cancel_hot = hover == Some(rid(R_CANCEL, 0));
            p.pill(
                &cancel,
                cancel_x,
                by,
                bw,
                36.0,
                if cancel_hot { pal.pressed } else { pal.surface },
                pal.text,
                13.0,
            );
            p.hover(rid(R_CANCEL, 0), cancel_x, by, bw, 36.0);
            if let Some(on) = always {
                let region = rid(R_ALWAYS, 0);
                let hot = hover == Some(region);
                let bx = 28.0;
                let size = 18.0;
                let cy = by + 18.0;
                p.round_stroke(
                    bx,
                    cy - size / 2.0,
                    size,
                    size,
                    4.0,
                    if on { pal.control } else { pal.surface },
                    if on || hot {
                        pal.control
                    } else {
                        pal.line_strong
                    },
                );
                if on {
                    p.glyph(
                        g::CHECK,
                        bx,
                        cy - size / 2.0,
                        size,
                        size,
                        11.0,
                        pal.on_control,
                    );
                }
                p.text(
                    "本次运行不再询问",
                    bx + 26.0,
                    cy - 12.0,
                    cancel_x - bx - 30.0,
                    24.0,
                    12.5,
                    pal.text2,
                );
                p.hover(region, bx, cy - 12.0, 150.0, 24.0);
            }
            p.finish()
        })
        .height(Dp::new(72.0)),
    );
    // The card's rounded surface sits under its transparent content.
    let surface_fill = canvas_with(move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let size = cx.size();
        p.round_stroke(
            0.0,
            0.0,
            size.width.0,
            size.height.0,
            16.0,
            pal.raised,
            pal.line_strong,
        );
        p.finish()
    });
    let card = grid(
        [ZsGridTrack::FLEX],
        [ZsGridTrack::FLEX],
        [
            ZsGridCell::new(0, 0, surface_fill),
            ZsGridCell::new(0, 0, column(parts)),
        ],
    )
    .width(Dp::new(DIALOG_W));
    column([
        spacer().flex(1.0),
        row([spacer().flex(1.0), card, spacer().flex(1.0)]),
        spacer().flex(1.2),
    ])
}

/// Handles approval clicks; returns `true` when the click was consumed.
pub fn approval_click(state: &mut State, region: u64) -> bool {
    if rsplit(region).0 == R_IMPORT_PLATFORM {
        if let Some(Modal::ImportPlaylist { platform, .. }) = &mut state.ui.modal
            && let Some(next) = crate::app::PLATFORMS.get(rsplit(region).1 as usize)
        {
            *platform = *next;
            return true;
        }
    }
    if state.ui.modal.is_some() {
        return false;
    }
    let (preview, busy) = {
        let store = state.bridge.store();
        (store.playlist_preview.is_some(), store.import_busy)
    };
    if preview || busy {
        match rsplit(region).0 {
            R_OK if preview => state.bridge.send(Job::CommitPlaylistImport),
            R_CANCEL => state.bridge.send(Job::CancelPlaylistImport),
            _ => {}
        }
        return true;
    }
    let pending = state.bridge.store().snapshot.pending_playback_url.is_some();
    if !pending {
        return false;
    }
    match rsplit(region).0 {
        R_OK => state
            .bridge
            .send(Job::Command(BackendCommand::ApprovePlaybackUrl)),
        R_CANCEL => state
            .bridge
            .send(Job::Command(BackendCommand::CancelPlaybackUrl)),
        R_ALWAYS => {
            let on = state.bridge.store().auto_approve_audio;
            state.bridge.send(Job::AutoApproveAudio(!on));
        }
        _ => return false,
    }
    true
}

fn shorten(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let head: String = text.chars().take(max).collect();
        format!("{head}…")
    }
}
