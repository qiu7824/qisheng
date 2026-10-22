//! These tests exercise real GPUI hit testing, text input and Slider events.
//! The command receiver replaces the controller, so no imported JS or network runs.
use gpui_kit::{
    AnyWindowHandle, AppContext, Bounds, Entity, TestAppContext, WindowBounds, WindowOptions,
    assets::AllAssets,
    point, px, size,
    test::{TestAppContextExt, TestWindowExt},
};
use lx_gpui_player::{
    ui::Ui,
    ui_data::{Page, SharedSnapshot, SourceView, TrackView, UiCommand, UiSnapshot},
};
use std::{
    sync::{Arc, Mutex, mpsc},
    time::Duration,
};

struct PlayerFixture {
    window: AnyWindowHandle,
    _view: Entity<Ui>,
    snapshot: SharedSnapshot,
    receiver: mpsc::Receiver<UiCommand>,
}

fn track() -> TrackView {
    TrackView {
        id: "original-tone".into(),
        title: "Original tone".into(),
        artist: "Local fixture".into(),
        album: "Generated audio".into(),
        duration: 120.,
        cover_color: 0x4a7970,
    }
}

fn source() -> SourceView {
    SourceView {
        id: "fixture".into(),
        name: "Original fixture".into(),
        version: "1.0".into(),
        author: "Local tests".into(),
        description: "Original synthetic audio, no external provider".into(),
        hash: "ab".repeat(32),
        enabled: false,
        approved: false,
        status: "Imported, awaiting approval".into(),
    }
}

fn fixture(cx: &mut TestAppContext, state: UiSnapshot) -> PlayerFixture {
    let snapshot = Arc::new(Mutex::new(state));
    let (sender, receiver) = mpsc::sync_channel(64);
    let shared = snapshot.clone();
    let (window, view) = cx.update(|cx| {
        gpui_kit::init(cx);
        gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Default::default(),
                    size: size(px(1280.), px(1200.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| Ui::new(window, cx, shared, sender)),
        )
        .expect("open native GPUI test window")
    });
    cx.update_window(window, |_, window, cx| window.render_frame(cx))
        .unwrap();
    PlayerFixture {
        window,
        _view: view,
        snapshot,
        receiver,
    }
}

impl PlayerFixture {
    fn commands(&self) -> Vec<UiCommand> {
        self.receiver.try_iter().collect()
    }
}

#[gpui_kit::test]
async fn navigation_and_search_use_real_events(cx: &mut TestAppContext) {
    let player = fixture(
        cx,
        UiSnapshot {
            page: Page::Library,
            tracks: vec![track()],
            total_tracks: 1,
            ..Default::default()
        },
    );
    cx.update_window(player.window, |_, window, cx| {
        window.click("nav-sources", cx);
    })
    .unwrap();
    assert!(
        player
            .commands()
            .iter()
            .any(|c| matches!(c, UiCommand::Navigate(Page::Sources)))
    );
    player.snapshot.lock().unwrap().page = Page::Sources;
    cx.wait_for(player.window, Duration::from_secs(1), |window, _| {
        window.try_find("import-source").is_some()
    })
    .await;
    cx.update_window(player.window, |_, window, cx| {
        window.click("library-search", cx);
        window.input("night", cx);
        assert_eq!(window.find("library-search").value(), Some("night"));
    })
    .unwrap();
    let commands = player.commands();
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::Search(q) if q == "night"))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::Navigate(Page::Library)))
    );
    cx.update_window(player.window, |_, window, cx| {
        window.click("nav-queue", cx);
        window.click("nav-library", cx);
    })
    .unwrap();
    let commands = player.commands();
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::Navigate(Page::Queue)))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::Navigate(Page::Library)))
    );
}

#[gpui_kit::test]
async fn source_import_approval_enable_disable_delete_are_separate_actions(
    cx: &mut TestAppContext,
) {
    let player = fixture(
        cx,
        UiSnapshot {
            page: Page::Sources,
            sources: vec![source()],
            ..Default::default()
        },
    );
    assert!(
        player.commands().is_empty(),
        "rendering an imported source must not execute or enable it"
    );
    cx.update_window(player.window, |_, window, cx| {
        assert!(window.try_find("enable-fixture").is_none());
        window.click("import-source", cx);
        window.click("add-demo-source", cx);
        window.click("approve-fixture", cx);
        window.click("delete-fixture", cx);
    })
    .unwrap();
    let commands = player.commands();
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::ImportSource))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::AddDemoSource))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::ApproveSource(id) if id == "fixture"))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::DeleteSource(id) if id == "fixture"))
    );
    player.snapshot.lock().unwrap().sources[0].approved = true;
    cx.wait_for(player.window, Duration::from_secs(1), |window, _| {
        window.try_find("enable-fixture").is_some()
    })
    .await;
    cx.update_window(player.window, |_, window, cx| {
        assert!(window.try_find("approve-fixture").is_none());
        window.click("enable-fixture", cx);
    })
    .unwrap();
    assert!(
        player
            .commands()
            .iter()
            .any(|c| matches!(c, UiCommand::EnableSource(id) if id == "fixture"))
    );
    player.snapshot.lock().unwrap().sources[0].enabled = true;
    cx.wait_for(player.window, Duration::from_secs(1), |window, _| {
        window.try_find("disable-fixture").is_some()
    })
    .await;
    cx.update_window(player.window, |_, window, cx| {
        window.click("disable-fixture", cx)
    })
    .unwrap();
    assert!(
        player
            .commands()
            .iter()
            .any(|c| matches!(c, UiCommand::DisableSource))
    );
}

#[gpui_kit::test]
fn playback_buttons_and_sliders_emit_real_commands(cx: &mut TestAppContext) {
    let song = track();
    let player = fixture(
        cx,
        UiSnapshot {
            tracks: vec![song.clone()],
            queue: vec![song.clone()],
            total_tracks: 1,
            now_playing: Some(song),
            playing: true,
            duration: 120.,
            position: 24.,
            volume: 0.7,
            ..Default::default()
        },
    );
    cx.update_window(player.window, |_, window, cx| {
        window.click("toggle", cx);
        window.click("previous", cx);
        window.click("next", cx);
        window.click("track-original-tone", cx);
        window.click("queue-toggle", cx);
        let mut seek = window.within("seek-control");
        let bounds = seek.find("slider-bar-container").bounds();
        seek.click_at(
            "slider-bar-container",
            point(bounds.size.width * 0.8, bounds.size.height * 0.5),
            cx,
        );
    })
    .unwrap();
    cx.update_window(player.window, |_, window, cx| {
        let mut volume = window.within("volume-control");
        let bounds = volume.find("slider-bar-container").bounds();
        volume.click_at(
            "slider-bar-container",
            point(bounds.size.width * 0.3, bounds.size.height * 0.5),
            cx,
        );
    })
    .unwrap();
    let commands = player.commands();
    assert!(commands.iter().any(|c| matches!(c, UiCommand::Toggle)));
    assert!(commands.iter().any(|c| matches!(c, UiCommand::Previous)));
    assert!(commands.iter().any(|c| matches!(c, UiCommand::Next)));
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::Play(id) if id == "original-tone"))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::Navigate(Page::Queue)))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::Seek(seconds) if (*seconds - 96.).abs() <= 1.)),
        "seek must release an absolute position in seconds: {commands:?}"
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::Volume(v) if (*v - 0.3).abs() <= 0.02)),
        "volume must receive the real Slider event: {commands:?}"
    );
}

#[gpui_kit::test]
fn empty_loading_and_error_feedback_preserve_disabled_actions(cx: &mut TestAppContext) {
    let player = fixture(
        cx,
        UiSnapshot {
            busy: true,
            error: Some("Fixture request failed".into()),
            ..Default::default()
        },
    );
    cx.update_window(player.window, |_, window, cx| {
        assert!(window.find("busy-spinner").visible());
        assert!(window.find("empty-state").visible());
        assert_eq!(
            window.find("error-banner").label(),
            Some("Fixture request failed")
        );
        window.click("play-library", cx);
        window.click("toggle", cx);
        window.click("import-audio", cx);
    })
    .unwrap();
    assert!(
        player.commands().is_empty(),
        "busy or empty disabled buttons must not dispatch actions"
    );
    cx.update_window(player.window, |_, window, cx| {
        window.click("clear-error", cx)
    })
    .unwrap();
    assert!(
        player
            .commands()
            .iter()
            .any(|c| matches!(c, UiCommand::ClearError))
    );
}

#[gpui_kit::test]
fn remote_audio_requires_explicit_confirmation(cx: &mut TestAppContext) {
    let player = fixture(
        cx,
        UiSnapshot {
            pending_playback_url: Some("https://example.invalid/original.wav".into()),
            ..Default::default()
        },
    );
    assert!(player.commands().is_empty());
    cx.update_window(player.window, |_, window, cx| {
        assert!(window.find("url-confirmation").visible());
        window.click("cancel-url", cx);
        window.click("approve-url", cx);
    })
    .unwrap();
    let commands = player.commands();
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::CancelPlaybackUrl))
    );
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, UiCommand::ApprovePlaybackUrl))
    );
}

#[test]
fn headless_capture_support_is_reported_honestly() {
    let available = gpui_kit::platform::current_headless_renderer()
        .expect("query official renderer support")
        .is_some();
    #[cfg(target_os = "windows")]
    assert!(
        !available,
        "GPUI 0.3.7's Windows factory currently returns None"
    );
    println!(
        "Official GPUI headless renderer available: {available}; screenshots require a real renderer."
    );
}

#[test]
#[ignore = "Requires a native graphics device; run explicitly to capture actual DirectX/Metal frames"]
fn capture_real_gpui_library_png() {
    // Use the native Windows renderer's public readback path. The Windows
    // headless factory is absent, but WindowsWindow implements render_to_image.
    // This starts a new isolated, real controller with generated local audio.
    let userdata = tempfile::tempdir().unwrap();
    let controller =
        lx_gpui_player::controller::ControllerHandle::start(userdata.path().into()).unwrap();
    for _ in 0..40 {
        if controller.snapshot.lock().unwrap().total_tracks > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(controller.snapshot.lock().unwrap().total_tracks > 0);
    let directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/screenshots/v0.2");
    std::fs::create_dir_all(&directory).unwrap();
    let (completed, completion) = mpsc::channel::<Result<(), String>>();
    gpui_kit::application()
        .with_assets(AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            cx.spawn(async move |cx| {
            let result: Result<(), String> = async {
            for (page, name, height) in [
                (Page::Library, "library.png", 820.),
                (Page::Sources, "sources.png", 1000.),
                (Page::Queue, "queue.png", 820.),
            ] {
                if page == Page::Sources {
                    controller.commands.send(UiCommand::AddDemoSource).map_err(|e| e.to_string())?;
                    for _ in 0..40 {
                        if !controller.snapshot.lock().unwrap().sources.is_empty() {
                            break;
                        }
                        cx.background_executor().timer(Duration::from_millis(25)).await;
                    }
                    if controller.snapshot.lock().unwrap().sources.is_empty() { return Err("Original demo import did not complete".into()); }
                }
                controller.commands.send(UiCommand::Navigate(page)).map_err(|e| e.to_string())?;
                for _ in 0..40 {
                    if controller.snapshot.lock().unwrap().page == page {
                        break;
                    }
                    cx.background_executor().timer(Duration::from_millis(25)).await;
                }
                if controller.snapshot.lock().unwrap().page != page { return Err(format!("Navigation to {page:?} did not complete")); }
                let snapshot = controller.snapshot.clone();
                let sender = controller.commands.clone();
                let (window, _view) = cx.update(|cx| gpui_kit::open_window(
                    WindowOptions {
                        window_bounds: Some(WindowBounds::Windowed(Bounds {
                            origin: point(px(-10000.), px(-10000.)),
                            size: size(px(1280.), px(height)),
                        })),
                        focus: false,
                                // Win32 applies the requested bounds only when shown.
                                // Keep this non-activating window off-screen.
                                show: true,
                        ..Default::default()
                    },
                    cx,
                    |window, cx| cx.new(|cx| Ui::new(window, cx, snapshot, sender)),
                )).map_err(|e| e.to_string())?;
                // Let the production Win32 loop process WM_SIZE and resize the
                // swapchain. Reading inside the launch callback captures 1x1.
                cx.background_executor().timer(Duration::from_millis(500)).await;
                let (rendered, viewport, bounds) = cx
                    .update_window(window, |_, window, cx| {
                        window.render_frame(cx);
                        (window.render_to_image(), window.viewport_size(), window.bounds())
                    })
                    .map_err(|e| e.to_string())?;
                let image = rendered.map_err(|e| e.to_string())?;
                println!("Native capture {name}: {}x{}, viewport {viewport:?}, window {bounds:?}", image.width(), image.height());
                if image.width() <= 1000 || image.height() <= 600 { return Err(format!("Native capture {name} has invalid size {}x{} after resize", image.width(), image.height())); }
                image.save(directory.join(name)).map_err(|e| e.to_string())?;
            }
            Ok(())
            }.await;
            let _ = completed.send(result);
            cx.update(|cx| cx.quit());
            }).detach();
        });
    completion
        .recv_timeout(Duration::from_secs(10))
        .expect("native capture did not report completion")
        .unwrap();
}
