//! View composition.
//!
//! ```text
//! ┌ nav ────────┬ ‹ ›                         ┬ player ─────────── — □ × ┐
//! │ 栖声   │                              │ ┌──────────────────┐     │
//! │ [ 搜索 ]     │  the page                    │ │      cover       │     │
//! │ 发现         │                              │ └──────────────────┘     │
//! │ 歌单广场     │                              │ title            ♡       │
//! │ 排行榜       │                              │ 0:42 ━━━━━━○──── 3:16    │
//! │ 我的音乐…    │                              │  ⇄   ⏮   ( ▶ )   ⏭   ⋯   │
//! │ 歌单…        │                              │ 歌词  接下来        ⤢     │
//! │ ⚙ 设置 ⧉ 音源 │                              │ lyrics / queue           │
//! └─────────────┴──────────────────────────────┴──────────────────────────┘
//! ```
//!
//! Three columns: navigation, the page, and the player panel, which keeps
//! the playing record — cover, transport, lyrics, queue — in view while you
//! browse. The full-screen stage replaces all three. The interface itself is
//! monochrome; album art and the drawn covers bring the color. Every region
//! is a ZSUI canvas drawn by its module; text inputs and menus are ordinary
//! ZSUI controls placed over them.

pub mod charts;
pub mod collection;
pub mod covers;
pub mod discover;
pub mod nav;
pub mod now;
pub mod overlay;
pub mod panel;
pub mod search;
pub mod settings;
pub mod sources;
pub mod stage;
pub mod walls;

use crate::{
    app::{
        self, CMD_NEXT, CMD_PREVIOUS, CMD_QUIT, CMD_SHOW, CMD_TOGGLE, Key, Msg, Page, State,
        Surface, W_MENU,
    },
    paint::{Btn, P, measure, rid, rsplit},
    theme::{Palette, glyph as g, theme_mode, with_alpha},
};
use lx_gpui_player::audio::PlaybackStatus;
use zsui::{
    Color, Command, Dp, HorizontalAlign, TextWeight, TraySpec, ViewNode, ZsAccelerator,
    ZsAcceleratorKey, ZsCanvasLayoutContext, ZsCanvasScene, ZsCanvasWheelEvent, ZsGridCell,
    ZsGridTrack, ZsMediaSession, ZsMenuFlyoutAnchor, ZsNotification, ZsTaskbarButton,
    ZsTaskbarProgress, ZsTaskbarSpec, canvas_with, column, grid, menu_flyout, row, scroll, spacer,
};

pub const NAV_W: f32 = 224.0;
pub const PANEL_W: f32 = 320.0;
pub const TOOLBAR_H: f32 = 56.0;
/// Horizontal padding of a page.
pub const PAD: f32 = 32.0;

/// Shared region kinds, outside every module's own range.
pub const R_ART_ON: u32 = 900;
pub const R_RETRY: u32 = 901;

const T_BACK: u32 = 1;
const T_FORWARD: u32 = 2;

/// An interactive canvas whose pointer and hover messages carry `surface`.
pub fn surface<F>(surface: Surface, build: F) -> ViewNode<Msg>
where
    F: Fn(&ZsCanvasLayoutContext<'_>) -> ZsCanvasScene + Send + Sync + 'static,
{
    canvas_with(build)
        .id(surface.widget())
        .on_canvas_pointer_with(move |event| Msg::Ptr(surface, event))
        .on_canvas_hover_with(move |region| Msg::Hover(surface, region))
}

/// A plain colored block.
pub fn fill(color: Color) -> ViewNode<Msg> {
    let pal = Palette::get();
    canvas_with(move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let size = cx.size();
        p.fill(
            0.0,
            0.0,
            size.width.0.max(1.0),
            size.height.0.max(1.0),
            color,
        );
        p.finish()
    })
}

/// Stacks nodes in one grid cell, first at the bottom.
pub fn layers(nodes: impl IntoIterator<Item = ViewNode<Msg>>) -> ViewNode<Msg> {
    grid(
        [ZsGridTrack::FLEX],
        [ZsGridTrack::FLEX],
        nodes
            .into_iter()
            .map(|node| ZsGridCell::new(0, 0, node))
            .collect::<Vec<_>>(),
    )
}

/// Places `node` over a full-size fill, so rows holding native text inputs
/// show the page surface instead of the theme container color.
pub fn backed(color: Color, node: ViewNode<Msg>) -> ViewNode<Msg> {
    layers([fill(color), node])
}

/// A vertical scroller whose offset is kept in the UI state under `key`.
pub fn scroller(state: &State, key: &str, child: ViewNode<Msg>) -> ViewNode<Msg> {
    let key = key.to_string();
    let offset = state.scroll(&key);
    scroll(child)
        .scroll_y(Dp::new(offset))
        .on_scroll_with(move |offset| Msg::Scroll(key.clone(), offset))
}

/// A single-line text input vertically centered in a band of `height`.
pub fn centered(node: ViewNode<Msg>, height: f32) -> ViewNode<Msg> {
    column([spacer().flex(1.0), node, spacer().flex(1.0)]).height(Dp::new(height))
}

pub fn root(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let main = if state.ui.stage {
        stage::view(state)
    } else {
        row([
            nav::view(state).width(Dp::new(NAV_W)),
            backed(pal.surface, column([toolbar(state), page(state).flex(1.0)])).flex(1.0),
            panel::view(state).width(Dp::new(PANEL_W)),
        ])
    };
    let body = backed(pal.surface, main);
    let layered = overlay::layer(state, body);
    let now = now::now(state);
    let mut root = shortcuts(layered)
        .theme_mode(theme_mode())
        .on_window_state_change(Msg::WindowState)
        .on_system_theme_change(Msg::Theme)
        .on_files_dropped(Msg::Dropped)
        .on_second_instance(Msg::Relaunched)
        .on_media_command(Msg::Media)
        .taskbar(taskbar(&now));
    if state.has_tray {
        root = root.status_item(tray(state));
    }
    if let Some(session) = media_session(&now) {
        root = root.media_session(session);
    }
    if let Some(notification) = notification(state, &now) {
        root = root.notification(notification);
    }

    let (open, target, anchor, spec) = match &state.ui.menu {
        Some(menu) => (
            true,
            menu.target.widget(),
            Some(ZsMenuFlyoutAnchor::new(Dp::new(menu.x), Dp::new(menu.y))),
            collection::menu_spec(state, menu),
        ),
        None => (false, Surface::List.widget(), None, zsui::MenuSpec::new()),
    };
    menu_flyout(W_MENU, open, target, spec, root)
        .menu_flyout_anchor(anchor)
        .on_menu_flyout_command(Msg::Menu)
        .on_menu_flyout_open_change(Msg::MenuOpen)
}

fn page(state: &State) -> ViewNode<Msg> {
    match state.ui.page.clone() {
        Page::Discover => discover::view(state),
        Page::Playlists => walls::playlists(state),
        Page::Charts => charts::view(state),
        Page::Search => search::view(state),
        Page::Albums => walls::albums(state),
        Page::Artists => walls::artists(state),
        Page::Songs | Page::Group(..) | Page::List(_) | Page::Playlist(..) => {
            collection::view(state)
        }
        Page::Sources => sources::view(state),
        Page::Settings => settings::page(state),
    }
}

const FILTER_W: f32 = 220.0;
const T_FILTER: u32 = 3;

/// The strip above the page: back and forward, the list filter of
/// collection pages, and room to drag the window.
fn toolbar(state: &State) -> ViewNode<Msg> {
    let pal = Palette::get();
    let hover = state.hover(Surface::Toolbar);
    let can_back = !state.ui.back.is_empty();
    let can_forward = !state.ui.forward.is_empty();
    let page = &state.ui.page;
    let filter_on = page.is_collection()
        && *page != Page::Charts
        && (!state.ui.filter.is_empty() || state.rows.rows(state, page).total > 0);
    let canvas = surface(Surface::Toolbar, move |cx: &ZsCanvasLayoutContext<'_>| {
        let mut p = P::new(pal);
        let w = cx.size().width.0;
        p.fill(0.0, 0.0, w, TOOLBAR_H, pal.surface);
        if filter_on {
            let (fx, fy) = (w - PAD - FILTER_W, TOOLBAR_H / 2.0 - 16.0 + 2.0);
            p.round(fx, fy, FILTER_W, 32.0, 16.0, pal.raised);
            p.glyph(g::SEARCH, fx + 4.0, fy, 32.0, 32.0, 11.0, pal.text3);
            p.hover_plain(rid(T_FILTER, 0), fx, fy, FILTER_W, 32.0);
        }
        for (kind, glyph, x, enabled, label) in [
            (T_BACK, g::CHEVRON_LEFT, PAD + 4.0, can_back, "后退"),
            (T_FORWARD, g::CHEVRON_RIGHT, PAD + 42.0, can_forward, "前进"),
        ] {
            let id = rid(kind, 0);
            let hot = enabled && hover == Some(id);
            p.icon_button(
                id,
                glyph,
                x,
                TOOLBAR_H / 2.0 + 2.0,
                32.0,
                10.5,
                enabled.then_some(if hot { pal.pressed } else { pal.raised }),
                if enabled { pal.text } else { pal.line_strong },
            );
            p.tip(id, label);
        }
        p.finish()
    })
    .window_drag_region();
    if !filter_on {
        return canvas.height(Dp::new(TOOLBAR_H));
    }
    let filter = zsui::textbox(state.ui.filter.clone())
        .id(app::W_FILTER)
        .placeholder("在列表中查找")
        .on_change(Msg::Filter)
        .chromeless()
        .width(Dp::new(FILTER_W - 48.0))
        .height(Dp::new(28.0));
    layers([
        canvas,
        row([
            spacer().flex(1.0),
            column([
                spacer().height(Dp::new(2.0)),
                centered(filter, TOOLBAR_H - 2.0),
            ])
            .width(Dp::new(FILTER_W - 48.0)),
            spacer().width(Dp::new(PAD + 14.0)),
        ]),
    ])
    .height(Dp::new(TOOLBAR_H))
}

pub fn toolbar_click(state: &mut State, region: u64, cx: &mut zsui::AppCx) {
    match rsplit(region).0 {
        T_BACK => app::act_go_back(state),
        T_FORWARD => app::act_go_forward(state),
        T_FILTER => cx.focus(app::W_FILTER),
        _ => {}
    }
}

/// Wheel input that a canvas handles itself rather than scrolling.
pub fn wheel(state: &mut State, surface: Surface, event: ZsCanvasWheelEvent) {
    match surface {
        Surface::Panel => panel::wheel(state, event),
        Surface::Stage => {
            let step = if event.delta.0 > 0.0 { -0.05 } else { 0.05 };
            app::act_set_volume(state, app::act_volume(state) + step);
        }
        _ => {}
    }
}

/// What the system media overlay, the lock screen and media keys act on.
fn media_session(now: &now::Now) -> Option<ZsMediaSession> {
    if !now.has_track {
        return None;
    }
    let mut session = ZsMediaSession::new(now.title.clone())
        .artist(now.artist.clone())
        .album(now.album.clone())
        .playing(now::is_active(now.status))
        .timeline(now.position, now.duration);
    if let Some(png) = now.cover.art().and_then(|art| art.png()) {
        session = session.artwork(png);
    }
    Some(session)
}

/// The transport under the taskbar thumbnail; the taskbar button shows
/// progress only while a track loads.
fn taskbar(now: &now::Now) -> ZsTaskbarSpec {
    let active = now::is_active(now.status);
    let loading = now.has_track
        && matches!(
            now.status,
            PlaybackStatus::Loading | PlaybackStatus::Buffering
        );
    ZsTaskbarSpec::new()
        .button(
            ZsTaskbarButton::new(g::PREVIOUS, "上一首", Command::custom(CMD_PREVIOUS))
                .enabled(now.has_track),
        )
        .button(ZsTaskbarButton::new(
            if active { g::PAUSE } else { g::PLAY },
            if active { "暂停" } else { "播放" },
            Command::custom(CMD_TOGGLE),
        ))
        .button(
            ZsTaskbarButton::new(g::NEXT, "下一首", Command::custom(CMD_NEXT))
                .enabled(now.has_track),
        )
        .progress(if loading {
            ZsTaskbarProgress::Indeterminate
        } else {
            ZsTaskbarProgress::None
        })
}

/// While the window is hidden to the notification area, each new track is
/// announced once, without a sound.
fn notification(state: &State, now: &now::Now) -> Option<ZsNotification> {
    if !state.has_track_notifications
        || !state.ui.hidden
        || !state.ui.notify_tracks
        || !now.has_track
    {
        return None;
    }
    {
        let mut hidden_track = state
            .ui
            .hidden_track
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if hidden_track.as_deref() == Some(now.id.as_str()) {
            return None;
        }
        // Once another track plays, the hidden one is news again.
        *hidden_track = None;
    }
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(&now.id, &mut hasher);
    let body = if now.artist.is_empty() {
        now.title.clone()
    } else {
        format!("{} — {}", now.title, now.artist)
    };
    Some(ZsNotification::new(std::hash::Hasher::finish(&hasher), "正在播放", body).silent(true))
}

/// The notification-area icon: the playing track as its tooltip and the
/// transport in its menu. A click brings the window back.
fn tray(state: &State) -> TraySpec {
    let (tooltip, playing) = {
        let store = state.bridge.store();
        let playing = now::is_active(store.snapshot.playback.status);
        let tooltip = match &store.snapshot.now_playing {
            Some(track) if !track.artist.is_empty() => {
                format!("{} - {}\n栖声", track.title, track.artist)
            }
            Some(track) => format!("{}\n栖声", track.title),
            None => "栖声".to_string(),
        };
        (tooltip, playing)
    };
    // Windows limits tray tooltips to 127 characters.
    let tooltip: String = tooltip.chars().take(120).collect();
    TraySpec::new()
        .tooltip(tooltip)
        .on_activate(Command::custom(CMD_SHOW))
        .item(
            if playing { "暂停" } else { "播放" },
            Command::custom(CMD_TOGGLE),
        )
        .item("上一首", Command::custom(CMD_PREVIOUS))
        .item("下一首", Command::custom(CMD_NEXT))
        .separator()
        .item("显示主窗口", Command::custom(CMD_SHOW))
        .item("退出 栖声", Command::custom(CMD_QUIT))
}

fn shortcuts(root: ViewNode<Msg>) -> ViewNode<Msg> {
    use ZsAcceleratorKey as K;
    let ctrl = ZsAccelerator::primary;
    let plain = ZsAccelerator::new;
    [
        (ctrl(K::Space), Key::PlayPause),
        (plain(K::MediaPlayPause), Key::PlayPause),
        (plain(K::MediaNextTrack), Key::Next),
        (plain(K::MediaPreviousTrack), Key::Previous),
        (ctrl(K::Right), Key::Next),
        (ctrl(K::Left), Key::Previous),
        (ctrl(K::Up), Key::VolumeUp),
        (ctrl(K::Down), Key::VolumeDown),
        (ZsAccelerator::primary_character('f'), Key::Search),
        (ZsAccelerator::primary_character('l'), Key::Stage),
        (plain(K::Escape), Key::Escape),
        (plain(K::Left).with_alt(), Key::Back),
        (plain(K::Right).with_alt(), Key::Forward),
        (plain(K::Right).shifted(), Key::SeekForward),
        (plain(K::Left).shifted(), Key::SeekBackward),
    ]
    .into_iter()
    .fold(root, |node, (accelerator, key)| {
        node.shortcut(accelerator, Msg::Key(key))
    })
}

/// A page title.
pub fn page_title(p: &mut P, x: f32, y: f32, w: f32, title: &str) {
    let pal = p.pal;
    p.heading(title, x - 1.0, y, w, 44.0, 30.0, pal.text);
}

/// A quiet empty state: a glyph in a soft circle, a title and a hint,
/// centered in `w`.
#[allow(clippy::too_many_arguments)]
pub fn empty_state(p: &mut P, x: f32, y: f32, w: f32, glyph: char, title: &str, hint: &str) {
    let pal = p.pal;
    let cx = x + w / 2.0;
    p.circle(cx, y + 32.0, 32.0, pal.raised);
    p.glyph(glyph, cx - 32.0, y, 64.0, 64.0, 22.0, pal.text3);
    p.text_ex(
        title,
        x,
        y + 80.0,
        w,
        26.0,
        15.0,
        pal.text,
        HorizontalAlign::Center,
        TextWeight::Semibold,
    );
    if !hint.is_empty() {
        p.text_c(hint, x, y + 106.0, w, 22.0, 12.5, pal.text3);
    }
}

/// A loading placeholder block.
pub fn skeleton(p: &mut P, x: f32, y: f32, w: f32, h: f32, radius: f32) {
    let pal = p.pal;
    p.round(x, y, w, h, radius, pal.raised);
}

/// An error line with a retry link; returns its height.
pub fn failure(
    p: &mut P,
    cx: &ZsCanvasLayoutContext<'_>,
    x: f32,
    y: f32,
    w: f32,
    message: &str,
    hover: Option<u64>,
) -> f32 {
    let pal = p.pal;
    p.glyph(g::WARNING, x, y, 20.0, 32.0, 12.0, pal.text3);
    let tw = measure(cx, message, 13.0, TextWeight::Regular).min(w - 140.0);
    p.text(message, x + 26.0, y, tw + 4.0, 32.0, 13.0, pal.text2);
    p.button(
        cx,
        rid(R_RETRY, 0),
        "重试",
        None,
        x + 40.0 + tw,
        y + 2.0,
        28.0,
        Btn::Secondary,
        hover,
    );
    40.0
}

/// The invitation to load online covers, shown on online pages until the
/// user allows it; returns its height including the gap below.
pub fn art_banner(
    p: &mut P,
    cx: &ZsCanvasLayoutContext<'_>,
    x: f32,
    y: f32,
    w: f32,
    hover: Option<u64>,
) -> f32 {
    let pal = p.pal;
    let h = 64.0;
    p.round(x, y, w, h, 12.0, pal.raised);
    p.circle(x + 32.0, y + h / 2.0, 16.0, with_alpha(pal.text, 0x14));
    p.glyph(g::GLOBE, x + 16.0, y + 16.0, 32.0, 32.0, 13.0, pal.text2);
    p.text_b(
        "在线封面没有加载",
        x + 60.0,
        y + 12.0,
        w - 200.0,
        22.0,
        13.5,
        pal.text,
    );
    p.text(
        "开启后会从音乐平台下载歌单和歌曲的封面",
        x + 60.0,
        y + 33.0,
        w - 200.0,
        20.0,
        12.0,
        pal.text3,
    );
    let label = "开启封面";
    let bw = measure(cx, label, 12.5, TextWeight::Semibold) + 28.0;
    p.button(
        cx,
        rid(R_ART_ON, 0),
        label,
        None,
        x + w - bw - 16.0,
        y + (h - 30.0) / 2.0,
        30.0,
        Btn::Primary,
        hover,
    );
    h + 24.0
}

/// Shared clicks on the banner and retry links; returns whether it handled
/// the click.
pub fn shared_click(state: &mut State, region: u64) -> bool {
    match rsplit(region).0 {
        R_ART_ON => {
            app::act_set_auto_art(state, true);
            true
        }
        R_RETRY => {
            retry(state);
            true
        }
        _ => false,
    }
}

/// Asks again for what the current page shows after a failure.
fn retry(state: &mut State) {
    use crate::bridge::Job;
    let browse = state.ui.browse;
    match state.ui.page.clone() {
        Page::Discover | Page::Playlists => {
            app::act_job(
                state,
                Job::Playlists {
                    platform: browse,
                    tag: if state.ui.page == Page::Playlists {
                        state.ui.tag.clone()
                    } else {
                        String::new()
                    },
                    page: 1,
                },
            );
            app::act_job(state, Job::Charts(browse));
            app::act_job(state, Job::PlaylistTags(browse));
        }
        Page::Charts => app::act_job(state, Job::Charts(browse)),
        Page::Playlist(platform, id) => app::act_job(
            state,
            Job::PlaylistDetail {
                platform,
                id,
                page: 1,
            },
        ),
        _ => {}
    }
}

/// Three level bars marking the current track; still when paused.
pub fn bars(p: &mut P, x: f32, cy: f32, playing: bool, phase: u64, color: Color) {
    const REST: [f32; 3] = [0.55, 1.0, 0.75];
    for (i, rest) in REST.iter().enumerate() {
        let t = if playing {
            // Wrap first: an f32 cannot carry epoch milliseconds.
            let t_ms = (phase % 600_000) as f32;
            let wave = ((t_ms / 150.0) + i as f32 * 2.1).sin() * 0.5 + 0.5;
            0.4 + wave * 0.6
        } else {
            *rest
        };
        let h = 13.0 * t;
        p.round(x + i as f32 * 5.0, cy + 6.5 - h, 3.0, h, 1.5, color);
    }
}

/// Play counts as people say them: 9876, 1.2万, 3.4亿.
pub fn plays(count: u64) -> String {
    if count >= 100_000_000 {
        format!("{:.1}亿", count as f64 / 100_000_000.0)
    } else if count >= 10_000 {
        format!("{:.1}万", count as f64 / 10_000.0)
    } else {
        count.to_string()
    }
}
