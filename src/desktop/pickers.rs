//! Native pickers run outside the view callback and its non-reentrant input lock.

use crate::bridge::Job;
use lx_gpui_player::backend::BackendCommand;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use zsui::{FileDialogService, FileDialogSpec, NativeFileDialogService};

#[derive(Clone, Copy)]
pub enum Kind {
    Audio,
    Folder,
    Source,
    Playlist,
}

#[derive(Default)]
pub struct Pickers {
    active: Arc<AtomicBool>,
}

struct ActivePicker(Arc<AtomicBool>);

impl Drop for ActivePicker {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl Pickers {
    pub fn open(&self, kind: Kind, jobs: mpsc::Sender<Job>) -> std::io::Result<bool> {
        self.spawn(jobs, move || select(kind))
    }

    fn spawn(
        &self,
        jobs: mpsc::Sender<Job>,
        select: impl FnOnce() -> Result<Option<Job>, String> + Send + 'static,
    ) -> std::io::Result<bool> {
        if self.active.swap(true, Ordering::AcqRel) {
            return Ok(false);
        }
        let active = ActivePicker(self.active.clone());
        std::thread::Builder::new()
            .name("lx-file-picker".into())
            .spawn(move || {
                // The native dialog pumps messages, including focus events for
                // the main window. The view callback must already have returned.
                let result = select();
                drop(active);
                match result {
                    Ok(Some(job)) => {
                        let _ = jobs.send(job);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        let _ = jobs.send(Job::DialogError(error));
                    }
                }
            })?;
        Ok(true)
    }
}

fn select(kind: Kind) -> Result<Option<Job>, String> {
    let mut service = NativeFileDialogService::new();
    match kind {
        Kind::Audio => {
            let spec = FileDialogSpec::new("导入本地音乐")
                .filter(
                    "音频文件",
                    [
                        "*.mp3", "*.flac", "*.wav", "*.ogg", "*.oga", "*.m4a", "*.mp4", "*.aac",
                    ],
                )
                .allow_multiple(true);
            let paths = service.open_file_dialog(&spec).map_err(|e| e.to_string())?;
            Ok(paths.filter(|paths| !paths.is_empty()).map(|paths| {
                let count = paths.len();
                Job::CommandNotice(
                    BackendCommand::ImportAudio { paths },
                    format!("已导入 {count} 个文件"),
                )
            }))
        }
        Kind::Folder => service
            .pick_folder(&FileDialogSpec::new("添加音乐文件夹"))
            .map(|path| path.map(Job::AddLibraryRoot))
            .map_err(|e| e.to_string()),
        Kind::Playlist => {
            let spec = FileDialogSpec::new("导入歌单文件")
                .filter("LX 歌单文件", ["*.lxmc", "*.json", "*.gz"]);
            let paths = service.open_file_dialog(&spec).map_err(|e| e.to_string())?;
            Ok(paths
                .and_then(|paths| paths.into_iter().next())
                .map(Job::ImportPlaylistFile))
        }
        Kind::Source => {
            let spec =
                FileDialogSpec::new("导入 LX 自定义音源").filter("JavaScript 音源", ["*.js"]);
            let paths = service.open_file_dialog(&spec).map_err(|e| e.to_string())?;
            Ok(paths
                .and_then(|paths| paths.into_iter().next())
                .map(|path| {
                    Job::CommandNotice(
                        BackendCommand::ImportSource { path },
                        "音源已导入：批准并启用后才会运行".into(),
                    )
                }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait_closed(pickers: &Pickers) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while pickers.active.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "picker failed to retire");
            std::thread::yield_now();
        }
    }

    #[test]
    fn blocked_native_picker_releases_caller_and_rejects_a_second_dialog() {
        let pickers = Pickers::default();
        let (jobs, received) = mpsc::channel();
        let (entered, ready) = mpsc::channel();
        let (close, closed) = mpsc::channel();
        assert!(
            pickers
                .spawn(jobs.clone(), move || {
                    entered.send(()).unwrap();
                    closed.recv_timeout(Duration::from_secs(2)).unwrap();
                    Ok(Some(Job::AddLibraryRoot("selected-folder".into())))
                })
                .unwrap()
        );
        ready.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            !pickers
                .spawn(jobs.clone(), || panic!("second picker opened"))
                .unwrap()
        );
        assert!(received.try_recv().is_err());
        close.send(()).unwrap();
        assert!(
            matches!(received.recv_timeout(Duration::from_secs(2)).unwrap(), Job::AddLibraryRoot(path) if path == std::path::Path::new("selected-folder"))
        );
        assert!(pickers.spawn(jobs, || Ok(None)).unwrap());
        wait_closed(&pickers);
        assert!(received.try_recv().is_err());
    }

    #[test]
    fn cancelled_or_failed_picker_can_be_reopened_without_importing_a_file() {
        let pickers = Pickers::default();
        let (jobs, received) = mpsc::channel();
        assert!(pickers.spawn(jobs.clone(), || Ok(None)).unwrap());
        wait_closed(&pickers);
        assert!(received.try_recv().is_err());
        assert!(
            pickers
                .spawn(jobs.clone(), || Err("picker failed".into()))
                .unwrap()
        );
        assert!(
            matches!(received.recv_timeout(Duration::from_secs(2)).unwrap(), Job::DialogError(error) if error == "picker failed")
        );
        assert!(pickers.spawn(jobs, || Ok(None)).unwrap());
        wait_closed(&pickers);
    }
}
