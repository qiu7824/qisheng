use gpui_kit::*;
use lx_gpui_player::{backend::default_data_dir, controller::ControllerHandle, ui::Ui, worker};
use std::path::PathBuf;

fn main() {
    if worker::worker_main() {
        return;
    }
    if let Err(error) = run() {
        eprintln!("LX Native: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let mut data_dir = default_data_dir(false)?;
    let mut custom_data = false;
    let mut portable = false;
    let mut diagnose = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                data_dir = args
                    .next()
                    .map(PathBuf::from)
                    .ok_or_else(|| anyhow::anyhow!("--data-dir requires a path"))?;
                custom_data = true;
            }
            "--diagnose" => diagnose = true,
            "--portable" => portable = true,
            "--help" | "-h" => {
                println!(
                    "LX Native {}\nUsage: lx-gpui-player [--data-dir PATH | --portable] [--diagnose]\n--diagnose uses only original fixtures and exercises real audio output.",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            _ => anyhow::bail!("Unknown option: {arg}"),
        }
    }
    if portable && !custom_data {
        data_dir = default_data_dir(true)?;
    }
    if diagnose {
        if !custom_data {
            data_dir =
                std::env::temp_dir().join(format!("lx-gpui-diagnostic-{}", std::process::id()));
        }
        let report = lx_gpui_player::diagnostics::run(data_dir)?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        if report["result"] != "PASS" {
            std::process::exit(2);
        }
        return Ok(());
    }
    let controller = ControllerHandle::start(data_dir)?;
    gpui_kit::application()
        .with_assets(gpui_kit::assets::AllAssets)
        .run(move |cx| {
            gpui_kit::init(cx);
            gpui_kit::component::Theme::change(gpui_kit::component::ThemeMode::Dark, None, cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1280.0), px(820.0)),
                    cx,
                ))),
                window_min_size: Some(size(px(1080.0), px(720.0))),
                titlebar: Some(TitlebarOptions {
                    title: Some("LX Native · Music Player".into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, move |window, cx| {
                cx.new(|cx| Ui::new(window, cx, controller.snapshot, controller.commands))
            })
            .expect("Unable to open the player window");
            cx.activate(true);
        });
    Ok(())
}
