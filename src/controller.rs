//! Compatibility adapter from the existing GPUI commands to the native backend.
use crate::{
    backend::{
        BackendCommand, BackendConfig, BackendHandle, BackendSnapshot, CommandEnvelope,
        TrackSummary,
    },
    catalog::Track,
    ui_data::*,
};
use anyhow::Result;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

pub struct ControllerHandle {
    pub snapshot: SharedSnapshot,
    pub commands: CommandSender,
}
impl ControllerHandle {
    pub fn start(data_dir: PathBuf) -> Result<Self> {
        let backend = BackendHandle::start(BackendConfig::new(data_dir))?;
        let snapshot = Arc::new(Mutex::new(UiSnapshot::default()));
        let (commands, receiver) = mpsc::sync_channel(64);
        let shared = snapshot.clone();
        thread::Builder::new()
            .name("lx-ui-adapter".into())
            .spawn(move || {
                let mut page = Page::Library;
                let mut query = String::new();
                let mut request = 0_u64;
                let mut adapter_error = None;
                loop {
                    match receiver.recv_timeout(Duration::from_millis(50)) {
                        Ok(UiCommand::Navigate(next)) => page = next,
                        Ok(UiCommand::Search(text)) => {
                            query = text;
                            page = Page::Library;
                        }
                        Ok(command) => {
                            let mapped = translate(command);
                            match mapped {
                                Ok(Some(command)) => {
                                    request += 1;
                                    if matches!(
                                        command,
                                        BackendCommand::ImportSource { .. }
                                            | BackendCommand::AddDemoSource
                                    ) {
                                        page = Page::Sources;
                                    }
                                    if matches!(command, BackendCommand::ClearError) {
                                        adapter_error = None;
                                    }
                                    if let Err(error) = backend.execute(CommandEnvelope {
                                        request_id: format!("ui-{request}"),
                                        origin: "ui".into(),
                                        expected_revision: None,
                                        command,
                                    }) {
                                        adapter_error = Some(error.to_string());
                                    }
                                }
                                Ok(None) => {}
                                Err(error) => adapter_error = Some(error.to_string()),
                            }
                        }
                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                    let mut state = project(&backend.snapshot(), page, &query);
                    if adapter_error.is_some() {
                        state.error = adapter_error.clone();
                    }
                    *shared.lock().unwrap_or_else(|e| e.into_inner()) = state;
                }
                let _ = backend.shutdown(Duration::from_secs(5));
            })?;
        Ok(Self { snapshot, commands })
    }
}
fn translate(command: UiCommand) -> Result<Option<BackendCommand>> {
    Ok(Some(match command {
        UiCommand::Navigate(_) | UiCommand::Search(_) => return Ok(None),
        UiCommand::ImportSource => {
            #[cfg(feature = "ui")]
            {
                let Some(path) = rfd::FileDialog::new()
                    .set_title("导入 LX 自定义音源")
                    .add_filter("JavaScript 音源", &["js"])
                    .pick_file()
                else {
                    return Ok(None);
                };
                BackendCommand::ImportSource { path }
            }
            #[cfg(not(feature = "ui"))]
            {
                anyhow::bail!(
                    "File selection requires the UI; use BackendCommand::ImportSource with an explicit path"
                )
            }
        }
        UiCommand::ImportAudio => {
            #[cfg(feature = "ui")]
            {
                let Some(paths) = rfd::FileDialog::new()
                    .set_title("导入本地音乐")
                    .add_filter("音频", &["wav", "mp3", "flac", "ogg", "m4a", "aac"])
                    .pick_files()
                else {
                    return Ok(None);
                };
                BackendCommand::ImportAudio { paths }
            }
            #[cfg(not(feature = "ui"))]
            {
                anyhow::bail!(
                    "File selection requires the UI; use BackendCommand::ImportAudio with explicit paths"
                )
            }
        }
        UiCommand::AddDemoSource => BackendCommand::AddDemoSource,
        UiCommand::ApproveSource(id) => BackendCommand::ApproveSource { id },
        UiCommand::EnableSource(id) => BackendCommand::EnableSource { id },
        UiCommand::DisableSource => BackendCommand::DisableSource,
        UiCommand::DeleteSource(id) => BackendCommand::DeleteSource { id },
        UiCommand::Play(id) => BackendCommand::Play { id },
        UiCommand::Toggle => BackendCommand::Toggle,
        UiCommand::Previous => BackendCommand::Previous,
        UiCommand::Next => BackendCommand::Next,
        UiCommand::Seek(seconds) => BackendCommand::Seek {
            seconds: seconds as f64,
        },
        UiCommand::Volume(volume) => BackendCommand::Volume { volume },
        UiCommand::ClearError => BackendCommand::ClearError,
        UiCommand::ApprovePlaybackUrl => BackendCommand::ApprovePlaybackUrl,
        UiCommand::CancelPlaybackUrl => BackendCommand::CancelPlaybackUrl,
    }))
}
fn project(snapshot: &BackendSnapshot, page: Page, query: &str) -> UiSnapshot {
    let needle = query.trim().to_lowercase();
    UiSnapshot {
        page,
        query: query.into(),
        tracks: snapshot
            .tracks
            .iter()
            .filter(|track| {
                needle.is_empty()
                    || [&track.title, &track.artist, &track.album]
                        .iter()
                        .any(|text| text.to_lowercase().contains(&needle))
            })
            .map(summary_view)
            .collect(),
        queue: snapshot.queue.iter().map(summary_view).collect(),
        sources: snapshot
            .sources
            .iter()
            .map(|source| SourceView {
                id: source.id.clone(),
                name: source.name.clone(),
                version: source.version.clone(),
                author: source.author.clone(),
                description: source.description.clone(),
                hash: source.hash.clone(),
                enabled: source.enabled,
                approved: source.approved,
                status: source.status.clone(),
            })
            .collect(),
        total_tracks: snapshot.tracks.len(),
        now_playing: snapshot.now_playing.as_ref().map(summary_view),
        status: match snapshot.playback.status {
            crate::audio::PlaybackStatus::Idle => "就绪",
            crate::audio::PlaybackStatus::Loading => "载入音频…",
            crate::audio::PlaybackStatus::Buffering => "缓冲中",
            crate::audio::PlaybackStatus::Playing => "正在播放",
            crate::audio::PlaybackStatus::Paused => "已暂停",
            crate::audio::PlaybackStatus::Ended => "播放完成",
            crate::audio::PlaybackStatus::Error => "播放失败",
        }
        .into(),
        playing: matches!(
            snapshot.playback.status,
            crate::audio::PlaybackStatus::Playing | crate::audio::PlaybackStatus::Buffering
        ),
        busy: snapshot.busy,
        position: snapshot.playback.position_seconds as f32,
        duration: snapshot.playback.duration_seconds as f32,
        volume: snapshot.playback.volume,
        error: snapshot.error.clone(),
        notice: snapshot.notice.clone(),
        pending_playback_url: snapshot.pending_playback_url.clone(),
    }
}
fn summary_view(track: &TrackSummary) -> TrackView {
    let colors = [0x74bba2, 0xb499db, 0x86adc6, 0xd2ac81];
    let color = track
        .id
        .bytes()
        .fold(0usize, |sum, byte| sum.wrapping_add(byte as usize))
        % colors.len();
    TrackView {
        id: track.id.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        duration: track.duration as f32,
        cover_color: colors[color],
    }
}
pub fn track_view(track: &Track) -> TrackView {
    summary_view(&TrackSummary::from(track))
}
pub fn read_script(path: &std::path::Path) -> Result<String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take((crate::source::MAX_SCRIPT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= crate::source::MAX_SCRIPT_BYTES,
        "音源文件超过 1 MiB 限制"
    );
    String::from_utf8(bytes).map_err(Into::into)
}
