//! Native GPUI view. The adapter selects files; the backend handles data and playback.
use std::time::Duration;

use gpui_kit::{
    assets::IconName,
    component::{
        Disableable, Icon, Sizable,
        button::{Button, ButtonVariants},
        input::{Input, InputEvent, InputState},
        slider::{Slider, SliderEvent, SliderState},
        spinner::Spinner,
    },
    prelude::*,
    *,
};

use crate::ui_data::{
    CommandSender, Page, SharedSnapshot, SourceView, TrackView, UiCommand, UiSnapshot,
};

const BG: u32 = 0x101416;
const PANEL: u32 = 0x181e21;
const LINE: u32 = 0x2a3336;
const TEXT: u32 = 0xf0f5f3;
const MUTED: u32 = 0x91a19e;
const ACCENT: u32 = 0x9be3bd;

pub struct Ui {
    snapshot: SharedSnapshot,
    commands: CommandSender,
    state: UiSnapshot,
    search: Entity<InputState>,
    progress: Entity<SliderState>,
    volume: Entity<SliderState>,
    seeking: bool,
    changing_volume: bool,
    _subscriptions: Vec<Subscription>,
    _refresh: Task<()>,
}

impl Ui {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        snapshot: SharedSnapshot,
        commands: CommandSender,
    ) -> Self {
        let state = snapshot.lock().map(|s| s.clone()).unwrap_or_default();
        let search = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("搜索曲名、歌手或专辑")
                .default_value(state.query.clone())
        });
        let progress = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(1.)
                .step(0.001)
                .default_value(if state.duration > 0. {
                    (state.position / state.duration).clamp(0., 1.)
                } else {
                    0.
                })
        });
        let volume = cx.new(|_| {
            SliderState::new()
                .min(0.)
                .max(1.)
                .step(0.01)
                .default_value(state.volume)
        });
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.subscribe_in(
            &search,
            window,
            |ui, input, event: &InputEvent, _, cx| {
                if matches!(event, InputEvent::Change) {
                    ui.send(UiCommand::Search(input.read(cx).value().to_string()));
                    if ui.state.page != Page::Library {
                        ui.send(UiCommand::Navigate(Page::Library));
                    }
                }
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &progress,
            window,
            |ui, _, event: &SliderEvent, _, cx| {
                match event {
                    SliderEvent::Change(_) => ui.seeking = true,
                    SliderEvent::Release(value) => {
                        ui.seeking = false;
                        if ui.state.duration > 0. {
                            ui.send(UiCommand::Seek(value.end() * ui.state.duration));
                        }
                    }
                }
                cx.notify();
            },
        ));
        subscriptions.push(cx.subscribe_in(
            &volume,
            window,
            |ui, _, event: &SliderEvent, _, cx| {
                match event {
                    SliderEvent::Change(value) => {
                        ui.changing_volume = true;
                        ui.send(UiCommand::Volume(value.end()));
                    }
                    SliderEvent::Release(value) => {
                        ui.changing_volume = false;
                        ui.send(UiCommand::Volume(value.end()));
                    }
                }
                cx.notify();
            },
        ));
        let refresh = cx.spawn_in(window, async move |view, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(200))
                    .await;
                if view
                    .update_in(cx, |ui, window, cx| {
                        if let Ok(state) = ui.snapshot.lock() {
                            ui.state = state.clone();
                        }
                        if !ui.seeking {
                            let fraction = if ui.state.duration > 0. {
                                (ui.state.position / ui.state.duration).clamp(0., 1.)
                            } else {
                                0.
                            };
                            ui.progress
                                .update(cx, |slider, cx| slider.set_value(fraction, window, cx));
                        }
                        if !ui.changing_volume {
                            ui.volume.update(cx, |slider, cx| {
                                slider.set_value(ui.state.volume, window, cx)
                            });
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            snapshot,
            commands,
            state,
            search,
            progress,
            volume,
            seeking: false,
            changing_volume: false,
            _subscriptions: subscriptions,
            _refresh: refresh,
        }
    }

    fn send(&self, command: UiCommand) {
        let _ = self.commands.try_send(command);
    }

    fn button(
        &self,
        id: impl Into<SharedString>,
        label: impl Into<SharedString>,
        icon: IconName,
        command: UiCommand,
        primary: bool,
        disabled: bool,
    ) -> Button {
        let commands = self.commands.clone();
        Button::new(id.into())
            .label(label)
            .icon(Icon::new(icon).size(px(16.)))
            .when(primary, |b| b.primary().bg(rgb(ACCENT)).text_color(rgb(BG)))
            .when(!primary, |b| {
                b.ghost().bg(rgb(0x252e31)).text_color(rgb(TEXT))
            })
            .rounded(px(10.))
            .h(px(38.))
            .disabled(disabled)
            .when(disabled, |b| b.opacity(0.45))
            .on_click(move |_, _, _| {
                let _ = commands.try_send(command.clone());
            })
    }

    fn icon_button(
        &self,
        id: &'static str,
        label: &'static str,
        icon: IconName,
        command: UiCommand,
        large: bool,
        disabled: bool,
    ) -> Button {
        let commands = self.commands.clone();
        Button::new(id)
            .child(Icon::new(icon).size(px(if large { 22. } else { 19. })))
            .tooltip(label)
            .accessibility_label(label)
            .ghost()
            .rounded(px(24.))
            .size(px(if large { 46. } else { 36. }))
            .when(large, |b| b.bg(rgb(ACCENT)).text_color(rgb(BG)))
            .when(!large, |b| b.text_color(rgb(TEXT)))
            .disabled(disabled)
            .when(disabled, |b| b.opacity(0.45))
            .on_click(move |_, _, _| {
                let _ = commands.try_send(command.clone());
            })
    }

    fn navigation(
        &self,
        page: Page,
        label: &'static str,
        icon: IconName,
        count: usize,
    ) -> AnyElement {
        let selected = self.state.page == page;
        let commands = self.commands.clone();
        div()
            .id(match page {
                Page::Library => "nav-library",
                Page::Sources => "nav-sources",
                Page::Queue => "nav-queue",
            })
            .test_support()
            .aria_label(label)
            .flex()
            .items_center()
            .gap_3()
            .h(px(45.))
            .px_3()
            .rounded(px(11.))
            .cursor_pointer()
            .bg(rgb(if selected { 0x263c33 } else { PANEL }))
            .text_color(rgb(if selected { ACCENT } else { MUTED }))
            .hover(|s| s.bg(rgb(0x25302d)).text_color(rgb(TEXT)))
            .on_click(move |_, _, _| {
                let _ = commands.try_send(UiCommand::Navigate(page));
            })
            .child(Icon::new(icon).size(px(19.)))
            .child(
                div()
                    .flex_1()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(label),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(count.to_string()),
            )
            .into_any_element()
    }

    fn sidebar(&self) -> AnyElement {
        let active = self.state.sources.iter().find(|s| s.enabled);
        div()
            .w(px(212.))
            .flex_shrink_0()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .border_r_1()
            .border_color(rgb(LINE))
            .p_5()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .mt_2()
                    .mb(px(38.))
                    .child(
                        div()
                            .size(px(37.))
                            .rounded(px(12.))
                            .bg(rgb(ACCENT))
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_color(rgb(BG))
                            .child(Icon::new(IconName::AudioLines).size(px(22.))),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(
                                div()
                                    .text_size(px(20.))
                                    .font_weight(FontWeight::BOLD)
                                    .child("LX 原生"),
                            )
                            .child(
                                div()
                                    .text_size(px(9.))
                                    .text_color(rgb(MUTED))
                                    .child("MUSIC, IN FOCUS"),
                            ),
                    ),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .mb_3()
                    .child("我的音乐"),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(self.navigation(
                        Page::Library,
                        "曲库",
                        IconName::Library,
                        self.state.total_tracks,
                    ))
                    .child(self.navigation(
                        Page::Sources,
                        "音源管理",
                        IconName::Radio,
                        self.state.sources.len(),
                    ))
                    .child(self.navigation(
                        Page::Queue,
                        "播放队列",
                        IconName::ListMusic,
                        self.state.queue.len(),
                    )),
            )
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .border_t_1()
                    .border_color(rgb(LINE))
                    .pt_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .text_xs()
                            .child(
                                div()
                                    .size(px(6.))
                                    .rounded_full()
                                    .bg(rgb(if active.is_some() { ACCENT } else { MUTED })),
                            )
                            .child(
                                active
                                    .map(|s| s.name.clone())
                                    .unwrap_or_else(|| "尚未启用音源".into()),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child("Rust · GPUI Kit\n本地优先，专注聆听"),
                    ),
            )
            .into_any_element()
    }

    fn topbar(&self) -> AnyElement {
        div()
            .flex()
            .items_center()
            .justify_between()
            .gap_4()
            .h(px(76.))
            .flex_shrink_0()
            .px(px(30.))
            .border_b_1()
            .border_color(rgb(LINE))
            .child(
                Input::new(&self.search)
                    .id("library-search")
                    .w(px(320.))
                    .h(px(38.))
                    .prefix(
                        Icon::new(IconName::Search)
                            .size(px(16.))
                            .text_color(rgb(MUTED)),
                    )
                    .cleanable(true),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .when(self.state.busy, |d| {
                        d.child(
                            div()
                                .id("busy-spinner")
                                .test_support()
                                .aria_label("加载中")
                                .child(Spinner::new().small().color(rgb(ACCENT).into())),
                        )
                    })
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(self.state.status.clone()),
                    )
                    .child(self.button(
                        "import-audio",
                        "导入音频",
                        IconName::FolderOpen,
                        UiCommand::ImportAudio,
                        false,
                        self.state.busy,
                    )),
            )
            .into_any_element()
    }

    fn alerts(&self) -> AnyElement {
        div().flex().flex_col().gap_3()
            .when_some(self.state.error.clone(), |d, error| d.child(
                div().id("error-banner").test_support().aria_label(error.clone()).flex().items_start().gap_3().rounded(px(12.)).bg(rgb(0x392526)).border_1()
                    .border_color(rgb(0x754547)).p_3().text_color(rgb(0xf1b2ac))
                    .child(Icon::new(IconName::CircleAlert).size(px(18.)))
                    .child(div().flex_1().flex().flex_col().gap_1().text_sm()
                        .child(div().font_weight(FontWeight::SEMIBOLD).child("操作未完成"))
                        .child(error))
                    .child(self.icon_button("clear-error", "关闭错误", IconName::X, UiCommand::ClearError, false, false))))
            .when_some(self.state.notice.clone(), |d, notice| d.child(
                div().flex().items_center().gap_2().rounded(px(10.)).bg(rgb(0x1c3028)).p_3()
                    .text_color(rgb(ACCENT)).text_sm()
                    .child(Icon::new(IconName::CircleCheck).size(px(16.))).child(notice)))
            .when_some(self.state.pending_playback_url.clone(), |d, url| d.child(
                div().id("url-confirmation").test_support().aria_label("确认音频下载").flex().flex_col().gap_3().rounded(px(12.)).bg(rgb(0x302f23))
                    .border_1().border_color(rgb(0x6a6240)).p_4()
                    .child(div().flex().items_center().gap_2().font_weight(FontWeight::SEMIBOLD)
                        .child(Icon::new(IconName::ShieldCheck).size(px(18.))).child("确认音频下载"))
                    .child(div().text_sm().text_color(rgb(MUTED)).child("音源返回以下 URL。允许后将通过 HTTP GET 下载音频用于播放；请确认你有访问和播放权限。"))
                    .child(div().text_xs().p_3().rounded(px(8.)).bg(rgb(BG)).child(url))
                    .child(div().flex().gap_2()
                        .child(self.button("approve-url", "允许下载并播放", IconName::Play, UiCommand::ApprovePlaybackUrl, true, false))
                        .child(self.button("cancel-url", "取消", IconName::X, UiCommand::CancelPlaybackUrl, false, false)))))
            .into_any_element()
    }

    fn cover(color: u32, size: f32, title: &str) -> AnyElement {
        let initial: String = title.chars().take(1).collect();
        div()
            .relative()
            .size(px(size))
            .flex_shrink_0()
            .rounded(px(size * 0.16))
            .overflow_hidden()
            .bg(rgb(color))
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .absolute()
                    .size(px(size * 0.74))
                    .top(px(size * 0.12))
                    .left(px(size * 0.12))
                    .rounded_full()
                    .border(px(size * 0.095))
                    .border_color(rgba(0xffffff25)),
            )
            .child(
                div()
                    .absolute()
                    .size(px(size * 0.42))
                    .top(px(size * 0.28))
                    .left(px(size * 0.28))
                    .rounded_full()
                    .border(px(size * 0.035))
                    .border_color(rgba(0xffffff45)),
            )
            .child(
                div()
                    .text_size(px(size * 0.26))
                    .text_color(rgb(0xffffff))
                    .font_weight(FontWeight::BOLD)
                    .child(initial),
            )
            .into_any_element()
    }

    fn empty(&self, title: &'static str, description: &'static str, source: bool) -> AnyElement {
        div()
            .id("empty-state")
            .test_support()
            .aria_label(title)
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .py(px(48.))
            .w_full()
            .child(
                div()
                    .size(px(68.))
                    .rounded(px(22.))
                    .bg(rgb(0x26322c))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(ACCENT))
                    .child(
                        Icon::new(if source {
                            IconName::Radio
                        } else {
                            IconName::Disc3
                        })
                        .size(px(30.)),
                    ),
            )
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(div().text_sm().text_color(rgb(MUTED)).child(description))
            .when(source, |d| {
                d.child(self.button(
                    "empty-import-source",
                    "导入 .js 音源",
                    IconName::FileUp,
                    UiCommand::ImportSource,
                    true,
                    self.state.busy,
                ))
            })
            .when(!source && self.state.query.is_empty(), |d| {
                d.child(self.button(
                    "empty-import-audio",
                    "导入本地音频",
                    IconName::FolderOpen,
                    UiCommand::ImportAudio,
                    true,
                    self.state.busy,
                ))
            })
            .into_any_element()
    }

    fn track_row(&self, track: &TrackView, index: usize) -> AnyElement {
        let current = self
            .state
            .now_playing
            .as_ref()
            .is_some_and(|t| t.id == track.id);
        let commands = self.commands.clone();
        let id = track.id.clone();
        div()
            .id(SharedString::from(format!("track-{}", track.id)))
            .test_support()
            .aria_label(track.title.clone())
            .flex()
            .items_center()
            .gap_4()
            .h(px(72.))
            .px_3()
            .rounded(px(10.))
            .cursor_pointer()
            .bg(rgb(if current { 0x20332c } else { BG }))
            .hover(|d| d.bg(rgb(0x222c29)))
            .on_click(move |_, _, _| {
                let _ = commands.try_send(UiCommand::Play(id.clone()));
            })
            .child(
                div()
                    .w(px(24.))
                    .text_xs()
                    .text_color(rgb(if current { ACCENT } else { MUTED }))
                    .when(current, |d| {
                        d.child(
                            Icon::new(if self.state.playing {
                                IconName::AudioLines
                            } else {
                                IconName::Pause
                            })
                            .size(px(16.)),
                        )
                    })
                    .when(!current, |d| d.child(format!("{:02}", index + 1))),
            )
            .child(Self::cover(track.cover_color, 44., &track.title))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(rgb(if current { ACCENT } else { TEXT }))
                            .child(track.title.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(track.artist.clone()),
                    ),
            )
            .child(
                div()
                    .w(px(152.))
                    .truncate()
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(track.album.clone()),
            )
            .child(
                div()
                    .w(px(45.))
                    .text_xs()
                    .text_color(rgb(MUTED))
                    .child(time(track.duration)),
            )
            .child(
                Icon::new(IconName::Play)
                    .size(px(15.))
                    .text_color(rgb(if current { ACCENT } else { MUTED })),
            )
            .into_any_element()
    }

    fn tracks(&self, queue: bool) -> AnyElement {
        let tracks = if queue {
            &self.state.queue
        } else {
            &self.state.tracks
        };
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .mt_1()
                    .mb_3()
                    .child(
                        div()
                            .text_size(px(18.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(if queue {
                                "接下来播放"
                            } else if self.state.query.is_empty() {
                                "全部歌曲"
                            } else {
                                "搜索结果"
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(rgb(MUTED))
                            .child(format!("{} 首歌曲", tracks.len())),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .px_3()
                    .pb_2()
                    .border_b_1()
                    .border_color(rgb(LINE))
                    .text_size(px(10.))
                    .text_color(rgb(MUTED))
                    .child(div().w(px(24.)).child("#"))
                    .child(div().flex_1().child("歌曲 / 歌手"))
                    .child(div().w(px(152.)).child("专辑"))
                    .child(div().w(px(77.)).child("时长")),
            )
            .children(
                tracks
                    .iter()
                    .enumerate()
                    .map(|(i, track)| self.track_row(track, i)),
            )
            .when(tracks.is_empty(), |d| {
                d.child(self.empty(
                    if queue {
                        "队列还是空的"
                    } else if self.state.query.is_empty() {
                        "开始你的聆听"
                    } else {
                        "没有找到匹配歌曲"
                    },
                    if queue {
                        "从曲库选择一首歌曲，队列将自动建立。"
                    } else if self.state.query.is_empty() {
                        "导入本地音频，或使用随附的合法合成样例。"
                    } else {
                        "换个关键词试试；当前搜索范围是本地曲库。"
                    },
                    false,
                ))
            })
            .into_any_element()
    }

    fn library(&self) -> AnyElement {
        let featured = self.state.tracks.first();
        div()
            .flex()
            .flex_col()
            .gap_7()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_6()
                    .rounded(px(22.))
                    .bg(rgb(0x1e322a))
                    .p(px(28.))
                    .border_1()
                    .border_color(rgb(0x304b3d))
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(10.))
                                    .text_color(rgb(ACCENT))
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child("YOUR EVERYDAY SOUNDTRACK"),
                            )
                            .child(
                                div()
                                    .text_size(px(32.))
                                    .font_weight(FontWeight::BOLD)
                                    .child("让音乐回到前台"),
                            )
                            .child(
                                div().text_sm().text_color(rgb(0xb5c6bd)).child(
                                    "清晰的曲库，直接的控制。\n从一首歌开始，把节奏交给自己。",
                                ),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap_3()
                                    .mt_2()
                                    .child(self.button(
                                        "play-library",
                                        "开始播放",
                                        IconName::Play,
                                        UiCommand::Play(
                                            featured.map(|t| t.id.clone()).unwrap_or_default(),
                                        ),
                                        true,
                                        featured.is_none() || self.state.busy,
                                    ))
                                    .child(self.button(
                                        "manage-sources",
                                        "管理音源",
                                        IconName::Radio,
                                        UiCommand::Navigate(Page::Sources),
                                        false,
                                        false,
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .relative()
                            .w(px(172.))
                            .h(px(152.))
                            .flex_shrink_0()
                            .child(
                                div()
                                    .absolute()
                                    .top(px(4.))
                                    .left(px(28.))
                                    .size(px(136.))
                                    .rounded(px(22.))
                                    .bg(rgb(0x5d6e42)),
                            )
                            .child(div().absolute().top(px(16.)).left_0().child(Self::cover(
                                featured.map(|t| t.cover_color).unwrap_or(0x4a7970),
                                136.,
                                featured.map(|t| t.title.as_str()).unwrap_or("音"),
                            ))),
                    ),
            )
            .child(self.tracks(false))
            .into_any_element()
    }

    fn source_card(&self, source: &SourceView) -> AnyElement {
        let short_hash: String = source.hash.chars().take(16).collect();
        div()
            .flex()
            .flex_col()
            .gap_3()
            .p_5()
            .rounded(px(14.))
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(if source.enabled { 0x466a56 } else { LINE }))
            .child(
                div()
                    .flex()
                    .items_start()
                    .justify_between()
                    .gap_4()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .size(px(43.))
                                    .rounded(px(12.))
                                    .bg(rgb(0x2c4136))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_color(rgb(ACCENT))
                                    .child(Icon::new(IconName::Radio).size(px(21.))),
                            )
                            .child(
                                div()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(px(17.))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .child(source.name.clone()),
                                    )
                                    .child(
                                        div().text_xs().text_color(rgb(MUTED)).child(format!(
                                            "{} · {}",
                                            source.author, source.version
                                        )),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .px_3()
                            .py_1()
                            .rounded_full()
                            .bg(rgb(if source.enabled { 0x2b4837 } else { 0x2a3033 }))
                            .text_color(rgb(if source.enabled { ACCENT } else { MUTED }))
                            .child(if source.enabled {
                                "已启用"
                            } else if source.approved {
                                "已授权 · 停用"
                            } else {
                                "待授权"
                            }),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(MUTED))
                    .child(source.description.clone()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(if source.enabled { ACCENT } else { MUTED }))
                    .child(source.status.clone()),
            )
            .when(!source.approved, |d| {
                d.child(
                    div().text_xs().text_color(rgb(ACCENT)).child(
                        "此授权允许运行脚本及 HTTP(S) 联网、发送数据。只授权已检查的可信音源。",
                    ),
                )
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_3()
                    .pt_3()
                    .border_t_1()
                    .border_color(rgb(LINE))
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child(format!("SHA-256  {}…", short_hash)),
                    )
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .when(!source.approved, |d| {
                                d.child(self.button(
                                    format!("approve-{}", source.id),
                                    "授权并启用",
                                    IconName::ShieldCheck,
                                    UiCommand::ApproveSource(source.id.clone()),
                                    true,
                                    self.state.busy,
                                ))
                            })
                            .when(source.approved && !source.enabled, |d| {
                                d.child(self.button(
                                    format!("enable-{}", source.id),
                                    "启用",
                                    IconName::Play,
                                    UiCommand::EnableSource(source.id.clone()),
                                    true,
                                    self.state.busy,
                                ))
                            })
                            .when(source.enabled, |d| {
                                d.child(self.button(
                                    format!("disable-{}", source.id),
                                    "停用",
                                    IconName::Pause,
                                    UiCommand::DisableSource,
                                    false,
                                    self.state.busy,
                                ))
                            })
                            .child(self.button(
                                format!("delete-{}", source.id),
                                "删除",
                                IconName::Trash,
                                UiCommand::DeleteSource(source.id.clone()),
                                false,
                                self.state.busy,
                            )),
                    ),
            )
            .into_any_element()
    }

    fn sources(&self) -> AnyElement {
        div().flex().flex_col().gap_5()
            .child(div().flex().items_center().justify_between().gap_4()
                .child(div().flex().flex_col().gap_2()
                    .child(div().text_size(px(28.)).font_weight(FontWeight::BOLD).child("音源管理"))
                    .child(div().text_sm().text_color(rgb(MUTED)).child("导入 LX 自定义音源，校验后由你决定是否运行。")))
                .child(self.button("import-source", "导入 .js 音源", IconName::FileUp, UiCommand::ImportSource, true, self.state.busy)))
            .child(div().flex().flex_col().gap_3().p_5().rounded(px(14.)).bg(rgb(0x1b2927))
                .border_1().border_color(rgb(0x334842))
                .child(div().flex().items_center().gap_2().text_color(rgb(ACCENT)).font_weight(FontWeight::SEMIBOLD)
                    .child(Icon::new(IconName::ShieldCheck).size(px(19.))).child("在运行脚本前，了解授权范围"))
                .child(div().text_sm().text_color(rgb(0xb8c8c1)).child("导入仅校验与保存，不自动运行。点击“授权并启用”会运行 JS，并授予该脚本 HTTP(S) 联网能力。"))
                .child(div().text_sm().text_color(rgb(MUTED)).child("授权范围包含 GET/POST 请求，以及向服务器发送请求头、正文和表单数据。只授权已检查、可信且有权使用的音源；权限范围更新后需要重新授权。"))
                .child(div().text_sm().text_color(rgb(MUTED)).child("脚本没有文件、进程或系统环境权限，禁止读取其他程序的配置或凭证。具体已实现 API 与限制以当前兼容清单为准，未支持 API 会明确报错。"))
                .child(div().text_xs().text_color(rgb(MUTED)).child("自定义音源用于解析播放 URL；本地曲库搜索由播放器提供。示例音源使用项目合成音频与本机 HTTP。")))
            .child(div().flex().items_center().justify_between().p_4().rounded(px(12.)).bg(rgb(PANEL))
                .child(div().flex().flex_col().gap_1()
                    .child(div().text_sm().font_weight(FontWeight::SEMIBOLD).child("先体验完整的导入与播放流程"))
                    .child(div().text_xs().text_color(rgb(MUTED)).child("合法合成样例 · 本机 HTTP · 无第三方音源")))
                .child(self.button("add-demo-source", "导入示例音源", IconName::Plus, UiCommand::AddDemoSource, false, self.state.busy)))
            .children(self.state.sources.iter().map(|s| self.source_card(s)))
            .when(self.state.sources.is_empty(), |d| d.child(self.empty("还没有自定义音源", "导入一个 .js 脚本后，元数据和校验结果会出现在这里。", true)))
            .into_any_element()
    }

    fn queue(&self) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap_5()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .child(
                        div()
                            .text_size(px(28.))
                            .font_weight(FontWeight::BOLD)
                            .child("播放队列"),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(MUTED))
                            .child("点击歌曲立即播放，上一首和下一首沿队列切换。"),
                    ),
            )
            .child(self.tracks(true))
            .into_any_element()
    }

    fn player(&self) -> AnyElement {
        let track = self.state.now_playing.as_ref();
        let no_track = track.is_none();
        div()
            .flex()
            .items_center()
            .gap_6()
            .h(px(118.))
            .flex_shrink_0()
            .px_6()
            .bg(rgb(0x151c1c))
            .border_t_1()
            .border_color(rgb(0x35403a))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .w(px(252.))
                    .flex_shrink_0()
                    .child(Self::cover(
                        track.map(|t| t.cover_color).unwrap_or(0x31483e),
                        56.,
                        track.map(|t| t.title.as_str()).unwrap_or("音"),
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(
                                        track
                                            .map(|t| t.title.clone())
                                            .unwrap_or_else(|| "选择一首歌曲".into()),
                                    ),
                            )
                            .child(
                                div().truncate().text_xs().text_color(rgb(MUTED)).child(
                                    track
                                        .map(|t| t.artist.clone())
                                        .unwrap_or_else(|| "你的音乐，随时开始".into()),
                                ),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_5()
                            .child(self.icon_button(
                                "previous",
                                "上一首",
                                IconName::SkipBack,
                                UiCommand::Previous,
                                false,
                                self.state.queue.is_empty() || self.state.busy,
                            ))
                            .child(self.icon_button(
                                "toggle",
                                if self.state.playing {
                                    "暂停"
                                } else {
                                    "播放"
                                },
                                if self.state.playing {
                                    IconName::Pause
                                } else {
                                    IconName::Play
                                },
                                UiCommand::Toggle,
                                true,
                                no_track || self.state.busy,
                            ))
                            .child(self.icon_button(
                                "next",
                                "下一首",
                                IconName::SkipForward,
                                UiCommand::Next,
                                false,
                                self.state.queue.is_empty() || self.state.busy,
                            )),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_3()
                            .w_full()
                            .text_size(px(10.))
                            .text_color(rgb(MUTED))
                            .child(div().w(px(34.)).child(time(self.state.position)))
                            .child(
                                div().id("seek-control").test_support().flex_1().child(
                                    Slider::new(&self.progress)
                                        .horizontal()
                                        .w_full()
                                        .bg(rgb(ACCENT))
                                        .disabled(
                                            no_track
                                                || self.state.duration <= 0.
                                                || self.state.busy,
                                        ),
                                ),
                            )
                            .child(div().w(px(34.)).child(time(self.state.duration))),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_3()
                    .w(px(186.))
                    .flex_shrink_0()
                    .child(
                        Icon::new(if self.state.volume <= 0. {
                            IconName::VolumeX
                        } else {
                            IconName::Volume2
                        })
                        .size(px(19.))
                        .text_color(rgb(MUTED)),
                    )
                    .child(
                        div().id("volume-control").test_support().w(px(82.)).child(
                            Slider::new(&self.volume)
                                .horizontal()
                                .w_full()
                                .bg(rgb(ACCENT)),
                        ),
                    )
                    .child(self.icon_button(
                        "queue-toggle",
                        "打开播放队列",
                        IconName::ListMusic,
                        UiCommand::Navigate(Page::Queue),
                        false,
                        false,
                    )),
            )
            .into_any_element()
    }
}

impl Render for Ui {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .font_family("Microsoft YaHei UI")
            .text_size(px(14.))
            .child(
                div().flex().flex_1().min_h_0().child(self.sidebar()).child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .child(self.topbar())
                        .child(
                            div()
                                .id(match self.state.page {
                                    Page::Library => "library-scroll",
                                    Page::Sources => "sources-scroll",
                                    Page::Queue => "queue-scroll",
                                })
                                .test_support()
                                .flex_1()
                                .min_h_0()
                                .overflow_y_scroll()
                                .p(px(30.))
                                .child(div().flex().flex_col().gap_5().child(self.alerts()).child(
                                    match self.state.page {
                                        Page::Library => self.library(),
                                        Page::Sources => self.sources(),
                                        Page::Queue => self.queue(),
                                    },
                                )),
                        ),
                ),
            )
            .child(self.player())
    }
}

fn time(seconds: f32) -> String {
    let seconds = seconds.max(0.) as u32;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}
