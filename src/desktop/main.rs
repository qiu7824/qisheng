//! 栖声 desktop player built with ZSUI.
//!
//! Usage: 栖声.exe [--data-dir PATH | --portable] [FILE|FOLDER ...]
//! Smoke builds (`--features smoke`) also accept `--smoke OUT.png` followed by
//! scripted input such as `--click x,y`, `--move x,y`, `--type text`,
//! `--key enter|esc|…` and `--scroll x,y,dy`.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod art;
mod art_cache;
mod bridge;
mod fmt;
mod instance;
mod paint;
mod pickers;
mod rows;
mod theme;
mod views;

use lx_gpui_player::{
    backend::{BackendConfig, BackendHandle, default_data_dir},
    worker,
};
use std::{path::PathBuf, time::Duration};
use zsui::{UiInvalidationHandle, native_window};

fn main() {
    if worker::worker_main() {
        return;
    }
    if let Err(error) = run() {
        eprintln!("栖声: {error:#}");
        #[cfg(windows)]
        {
            use zsui::{
                DialogLevel, NativeDesktopDialogService, NativeDialogService, NativeDialogSpec,
            };
            let _ = NativeDesktopDialogService::new().show_native_dialog(
                &NativeDialogSpec::message("栖声 无法启动", format!("{error:#}"))
                    .level(DialogLevel::Error),
            );
        }
        std::process::exit(1);
    }
}

struct Args {
    data_dir: PathBuf,
    memory_audio: bool,
    #[cfg_attr(not(feature = "smoke"), allow(dead_code))]
    raw: Vec<String>,
}

fn args() -> anyhow::Result<Option<Args>> {
    let raw: Vec<String> = std::env::args().collect();
    let mut data_dir = None;
    let mut portable = false;
    let mut memory_audio = false;
    let mut iter = raw.iter().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--data-dir" => {
                data_dir =
                    Some(PathBuf::from(iter.next().ok_or_else(|| {
                        anyhow::anyhow!("--data-dir requires a path")
                    })?))
            }
            "--portable" => portable = true,
            // Renders audio into memory instead of a device (testing).
            "--audio" => {
                memory_audio = iter.next().map(String::as_str) == Some("memory");
            }
            "--help" | "-h" => {
                println!(
                    "栖声 {}\nUsage: 栖声.exe [--data-dir PATH | --portable] [--audio memory] [FILE|FOLDER ...]",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(None);
            }
            // Smoke-test input is parsed later; skip its values here.
            other if app::VALUE_OPTIONS.contains(&other) => {
                iter.next();
            }
            other if cfg!(feature = "smoke") && other.starts_with("--") => {}
            // Files and folders to open ("Open with").
            other if !other.starts_with("--") => {}
            other => anyhow::bail!("Unknown option: {other}"),
        }
    }
    let data_dir = match data_dir {
        Some(dir) => dir,
        None => default_data_dir(portable)?,
    };
    Ok(Some(Args {
        data_dir,
        memory_audio,
        raw,
    }))
}

fn run() -> anyhow::Result<()> {
    let Some(args) = args()? else {
        return Ok(());
    };
    let smoke = cfg!(feature = "smoke") && args.raw.iter().any(|a| a == "--smoke");
    let instance_smoke =
        cfg!(feature = "smoke") && args.raw.iter().any(|a| a == "--instance-smoke");
    let instance = instance::id_for_directory(&args.data_dir)?;
    let invalidate = UiInvalidationHandle::new();
    let inbox = instance::Inbox::default();
    let opened = app::launch_paths(&args.raw[1..]);
    let _instance_guard = if !smoke || instance_smoke {
        match instance::start(&instance, &opened, inbox.clone(), invalidate.clone())? {
            instance::Launch::Forwarded => return Ok(()),
            instance::Launch::Primary(guard) => Some(guard),
        }
    } else {
        None
    };
    let config = if args.memory_audio {
        BackendConfig::headless(args.data_dir.clone())
    } else {
        BackendConfig::new(args.data_dir.clone())
    };
    let backend = BackendHandle::start(config)?;
    let bridge = bridge::Bridge::start(backend.clone(), invalidate.clone());
    let mut state = app::State::new(bridge, args.data_dir.clone());
    state.instance_inbox = inbox.clone();
    if !opened.is_empty() {
        state.import_paths(opened);
    }

    let mut window = native_window("栖声")
        .size(1280, 800)
        .min_size(1100, 700)
        .custom_title_bar(true);
    if !smoke || instance_smoke {
        window = window.single_instance(instance);
    }
    if !smoke {
        if let Some(placement) = app::saved_placement(&args.data_dir) {
            window = window.placement(placement);
        }
    }
    let builder = window
        .invalidation_handle(invalidate)
        // Closing goes through the app, which may hide to the tray instead.
        .on_close_requested(zsui::Command::custom(app::CMD_CLOSE))
        .stateful_view_with_app_commands(state, app::view, app::update, app::app_command);

    #[cfg(feature = "smoke")]
    if args.raw.iter().any(|a| a == "--smoke") {
        let report = builder.run_smoke(smoke_options(&args.raw)?)?;
        inbox.close();
        println!("{:?}", report.screenshot_file);
        let _ = backend.shutdown(Duration::from_secs(5));
        if instance_smoke {
            anyhow::ensure!(
                inbox.handled_count() > 0,
                "No second launch reached the native UI"
            );
        }
        if let Some(path) = &report.screenshot_file {
            std::fs::write(
                std::path::Path::new(path).with_extension("interaction.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "native": report, "relaunches_handled": inbox.handled_count()
                }))?,
            )?;
        }
        return Ok(());
    }

    builder.run()?;
    inbox.close();
    backend
        .shutdown(Duration::from_secs(5))
        .map_err(|error| anyhow::anyhow!("backend shutdown: {error}"))?;
    Ok(())
}

#[cfg(feature = "smoke")]
fn smoke_options(raw: &[String]) -> anyhow::Result<zsui::NativeWindowSmokeRunOptions> {
    let value = |name: &str| {
        raw.iter()
            .position(|a| a == name)
            .and_then(|i| raw.get(i + 1))
            .cloned()
    };
    let file = value("--smoke").unwrap_or_else(|| "smoke.png".into());
    let ms = value("--smoke-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(2500);
    let mut options = zsui::NativeWindowSmokeRunOptions::new(ms)
        .screenshot_file(file)
        .require_screenshot(true);
    if raw.iter().any(|argument| argument == "--smoke-close") {
        options = options.native_window_close_request();
    }
    let point = |v: &str| -> anyhow::Result<zsui::Point> {
        let mut it = v.split(',').map(|n| n.trim().parse::<i32>().ok());
        match (it.next().flatten(), it.next().flatten()) {
            (Some(x), Some(y)) => Ok(zsui::Point { x, y }),
            _ => anyhow::bail!("expected x,y but got {v}"),
        }
    };
    let mut i = 1;
    while i + 1 < raw.len() {
        let (flag, value) = (raw[i].as_str(), raw[i + 1].as_str());
        match flag {
            "--click" => options = options.native_view_click(point(value)?),
            "--move" => options = options.native_view_pointer_move(point(value)?),
            "--wait" => options = options.native_view_wait(value.parse().unwrap_or(1000)),
            "--size" => {
                let n: Vec<i32> = value
                    .split(['x', ','])
                    .filter_map(|n| n.trim().parse().ok())
                    .collect();
                if n.len() == 2 {
                    options = options.native_window_resize(zsui::Size {
                        width: n[0],
                        height: n[1],
                    });
                }
            }
            "--rclick" => {
                let p = point(value)?;
                options = options.native_view_pointer_drag(
                    p,
                    p,
                    zsui::ZsPointerButton::Secondary,
                    zsui::ZsPointerModifiers::default(),
                );
            }
            "--type" => options = options.native_view_text_input(value),
            "--key" => {
                use zsui::NativeViewKey as K;
                let key = match value {
                    "enter" => K::Enter,
                    "esc" => K::Escape,
                    "tab" => K::Tab,
                    "space" => K::Space,
                    "up" => K::Up,
                    "down" => K::Down,
                    "left" => K::Left,
                    "right" => K::Right,
                    other => anyhow::bail!("--key does not support {other}"),
                };
                options = options.native_view_key_down(key);
            }
            "--scroll" => {
                let n: Vec<i32> = value
                    .split(',')
                    .filter_map(|n| n.trim().parse().ok())
                    .collect();
                if n.len() == 3 {
                    options = options.native_view_scroll(zsui::Point { x: n[0], y: n[1] }, n[2]);
                }
            }
            _ => {
                i += 1;
                continue;
            }
        }
        i += 2;
    }
    Ok(options)
}
