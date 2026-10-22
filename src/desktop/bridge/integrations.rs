use super::*;
use lx_gpui_player::{
    backend::{DeepLinkOperationState, DeepLinkPageMetadata},
    open_api::{ServerConfig, ServerState},
};
use std::path::PathBuf;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiView {
    pub state: ServerState,
    pub address: String,
    pub error: String,
}

pub(super) struct PendingImport {
    operation: String,
    file: bool,
    started: Instant,
    offset: usize,
    name: String,
    tracks: Vec<TrackRecord>,
}

pub fn playlist_url(platform: Platform, input: &str) -> Result<String, String> {
    let input = input.trim();
    if input.is_empty() || input.len() > 4096 {
        return Err("请输入歌单链接或 ID（最多 4096 字节）".into());
    }
    let mut url = reqwest::Url::parse("lxmusic://songlist/open").unwrap();
    let mut data = serde_json::json!({"source":platform.as_str()});
    let field = if input.starts_with("https://") || input.starts_with("http://") {
        "url"
    } else {
        "id"
    };
    data[field] = serde_json::Value::String(input.into());
    url.query_pairs_mut().append_pair("data", &data.to_string());
    lx_gpui_player::open_api::parse_deeplink(url.as_str()).map_err(|e| e.to_string())?;
    Ok(url.into())
}

impl Worker {
    pub(super) fn set_open_api(&mut self, enabled: bool, port: u16) {
        if let Some(server) = self.open_api.as_mut() {
            match server.shutdown(Duration::from_secs(3)) {
                Ok(true) => {
                    self.open_api.take();
                }
                Ok(false) => {
                    self.fail("接口仍在停止，请稍后重试");
                    return;
                }
                Err(error) => {
                    self.fail(error);
                    return;
                }
            }
        }
        self.store().api = ApiView::default();
        if enabled {
            match self.backend.start_open_api(ServerConfig {
                port,
                ..Default::default()
            }) {
                Ok(server) => {
                    self.open_api = Some(server);
                }
                Err(error) => {
                    self.store().api = ApiView {
                        state: ServerState::Failed,
                        error: friendly_error(&error.to_string()),
                        ..Default::default()
                    };
                    self.fail(error);
                }
            }
        }
        self.dirty = true;
        self.poll_api();
    }

    fn poll_api(&mut self) {
        let Some(server) = self.open_api.as_mut() else {
            return;
        };
        while server.try_recv().is_some() {}
        let view = server.view();
        let next = ApiView {
            state: view.state,
            address: view
                .address
                .map(|a| format!("http://{a}"))
                .unwrap_or_default(),
            error: view
                .last_error
                .map(|e| friendly_error(&e.to_string()))
                .unwrap_or_default(),
        };
        let changed = self.store().api != next;
        if changed {
            self.store().api = next;
            self.dirty = true;
        }
    }

    pub(super) fn begin_playlist_import(
        &mut self,
        file: Option<PathBuf>,
        link: Option<(Platform, String)>,
    ) {
        if self.import.is_some() || self.store().playlist_preview.is_some() {
            self.fail("请先完成或取消当前歌单导入");
            return;
        }
        let is_file = file.is_some();
        let command = if let Some(path) = file {
            BackendCommand::PreviewLxBackup {
                path,
                replace_tracks: true,
                copy_list_id: None,
            }
        } else if let Some((platform, input)) = link {
            let url = match playlist_url(platform, &input) {
                Ok(url) => url,
                Err(error) => {
                    self.fail(error);
                    return;
                }
            };
            BackendCommand::DispatchDeepLink { url }
        } else {
            return;
        };
        match self.execute(command) {
            Ok(operation) => {
                self.import = Some(PendingImport {
                    operation,
                    file: is_file,
                    started: Instant::now(),
                    offset: 0,
                    name: String::new(),
                    tracks: Vec::new(),
                });
                self.store().import_busy = true;
                self.dirty = true;
            }
            Err(error) => self.fail(error),
        }
    }

    pub(super) fn cancel_playlist_import(&mut self) {
        if let Some(task) = self.import.take() {
            let _ = self.execute(BackendCommand::CancelOperation {
                operation_id: task.operation,
            });
        }
        let mut store = self.store();
        store.import_busy = false;
        store.playlist_preview = None;
        drop(store);
        self.dirty = true;
    }

    pub(super) fn finish_import_failure(&mut self, operation: &str) {
        if self
            .import
            .as_ref()
            .is_some_and(|task| task.operation == operation)
        {
            self.import = None;
            self.store().import_busy = false;
            self.dirty = true;
        }
    }

    pub(super) fn commit_playlist_import(&mut self) {
        let preview = self.store().playlist_preview.take();
        if let Some(preview) = preview {
            if self.command(BackendCommand::CommitLxBackup {
                preview_operation_id: preview.operation_id,
            }) {
                self.refresh_lists();
                self.refresh_queue();
                self.notice(format!("歌单导入完成：{} 个歌单", preview.imported_lists));
            }
        }
        self.dirty = true;
    }

    pub(super) fn poll_integrations(&mut self) {
        self.poll_api();
        let Some(mut task) = self.import.take() else {
            return;
        };
        if task.started.elapsed() > Duration::from_secs(180) {
            let _ = self.execute(BackendCommand::CancelOperation {
                operation_id: task.operation,
            });
            self.store().import_busy = false;
            self.fail("歌单读取超时，请重试");
            return;
        }
        if task.file {
            if let Some(BackendQueryResult::LxBackupPreview(Some(preview))) =
                self.query(BackendQuery::LxBackupPreview)
                && preview.operation_id == task.operation
            {
                self.store().import_busy = false;
                if preview.settings_present {
                    self.fail("这是完整设置备份，请选择导出的歌单文件");
                } else if preview.imported_lists == 0 {
                    self.fail("文件中没有可导入的歌单");
                } else {
                    self.store().playlist_preview = Some(preview);
                    self.dirty = true;
                }
                return;
            }
            self.import = Some(task);
            return;
        }
        let page = match self.query(BackendQuery::DeepLinkResult {
            operation_id: task.operation.clone(),
            offset: task.offset,
            limit: 500,
        }) {
            Some(BackendQueryResult::DeepLinkResult(page)) => page,
            _ => {
                self.store().import_busy = false;
                return;
            }
        };
        match page.state {
            DeepLinkOperationState::Running => {
                self.import = Some(task);
            }
            DeepLinkOperationState::Completed => {
                let complete = match &page.metadata {
                    Some(DeepLinkPageMetadata::Songlist { info, complete, .. }) => {
                        task.name = info.name.clone();
                        *complete
                    }
                    _ => false,
                };
                if !complete || task.tracks.len().saturating_add(page.tracks.len()) > 50_000 {
                    self.store().import_busy = false;
                    self.fail("歌单未完整读取，未导入；请重试或拆分大歌单");
                    return;
                }
                task.tracks.extend(page.tracks);
                if page.has_more {
                    if page.next_offset <= task.offset {
                        self.store().import_busy = false;
                        self.fail("歌单分页没有前进，已停止导入");
                        return;
                    }
                    task.offset = page.next_offset;
                    self.import = Some(task);
                } else {
                    self.store().import_busy = false;
                    self.handle(Job::SaveAsList {
                        name: if task.name.is_empty() {
                            "导入的歌单".into()
                        } else {
                            task.name
                        },
                        tracks: task.tracks,
                    });
                    self.dirty = true;
                }
            }
            _ => {
                self.store().import_busy = false;
                self.fail(
                    page.error
                        .map(|e| e.message)
                        .unwrap_or_else(|| "歌单读取失败或已取消".into()),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn playlist_input_encodes_links_without_turning_import_into_playback() {
        use lx_gpui_player::open_api::{DeepLinkAction, parse_deeplink};
        for input in ["12345", "https://music.163.com/#/playlist?id=12345&x=中文"] {
            let url = playlist_url(Platform::Wy, input).unwrap();
            assert!(matches!(
                parse_deeplink(&url).unwrap().action,
                DeepLinkAction::SonglistOpen { .. }
            ));
        }
        assert!(playlist_url(Platform::Tx, " ").is_err());
        assert!(playlist_url(Platform::Tx, &"x".repeat(4097)).is_err());
    }

    struct Running {
        directory: tempfile::TempDir,
        backend: BackendHandle,
        bridge: Bridge,
    }
    impl Running {
        fn new(transport: Option<Arc<dyn HttpTransport>>) -> Self {
            let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
            let mut config =
                lx_gpui_player::backend::BackendConfig::headless(directory.path().join("profile"));
            config.provider_registry = transport.map(|transport| {
                Arc::new(lx_gpui_player::platforms::ProviderRegistry::new(transport))
            });
            let backend = BackendHandle::start(config).unwrap();
            let bridge = Bridge::start(backend.clone(), UiInvalidationHandle::new());
            Self {
                directory,
                backend,
                bridge,
            }
        }
        fn wait(&self, ready: impl Fn(&Store) -> bool) {
            let end = Instant::now() + Duration::from_secs(15);
            loop {
                if ready(&self.bridge.store()) {
                    return;
                }
                assert!(
                    Instant::now() < end,
                    "Desktop integration timed out: {:?}",
                    self.bridge.store().toasts
                );
                thread::sleep(Duration::from_millis(10));
            }
        }
    }
    impl Drop for Running {
        fn drop(&mut self) {
            self.bridge.shutdown();
            self.backend.shutdown(Duration::from_secs(15)).unwrap();
        }
    }

    #[test]
    fn desktop_api_jobs_start_lx_status_and_sse_then_release_the_listener() {
        use std::{
            io::{BufRead, BufReader, Read, Write},
            net::{TcpListener, TcpStream},
        };
        let running = Running::new(None);
        running.bridge.send(Job::SetOpenApi {
            enabled: true,
            port: 0,
        });
        running.wait(|s| s.api.state == ServerState::Listening);
        let url = running.bridge.store().api.address.clone();
        let address = url.strip_prefix("http://").unwrap();
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            socket,
            "GET /status?filter=status,name HTTP/1.1\r\nHost: {address}\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        socket.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        let json: serde_json::Value =
            serde_json::from_str(response.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(json["status"], "stoped");
        assert_eq!(json["name"], "");
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write!(
            socket,
            "GET /subscribe-player-status?filter=status HTTP/1.1\r\nHost: {address}\r\n\r\n"
        )
        .unwrap();
        let mut stream = BufReader::new(socket);
        let mut line = String::new();
        loop {
            line.clear();
            stream.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        line.clear();
        stream.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "event: status");
        line.clear();
        stream.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "data: \"stoped\"");
        running.bridge.send(Job::SetOpenApi {
            enabled: false,
            port: 0,
        });
        running.wait(|s| s.api.state == ServerState::Disabled);
        drop(stream);
        let listener = TcpListener::bind(address).unwrap();
        drop(listener);
    }

    #[test]
    fn desktop_file_import_requires_confirmation_and_survives_restart() {
        let mut running = Running::new(None);
        let path = running.directory.path().join("selected.lxmc");
        let input = serde_json::json!({"type":"playList_v2","data":[{"id":"desktop-import","name":"文件歌单","list":[{
            "id":"kw_original","source":"kw","name":"原创测试曲","singer":"作者",
            "meta":{"songId":"original","albumName":"专辑","_qualitys":{"128k":{}},"qualitys":[{"type":"128k"}]}
        }]}]});
        std::fs::write(&path, serde_json::to_vec(&input).unwrap()).unwrap();
        running.bridge.send(Job::ImportPlaylistFile(path.clone()));
        running.wait(|s| s.playlist_preview.is_some());
        assert!(
            !running
                .bridge
                .store()
                .lists
                .iter()
                .any(|list| list.id == "desktop-import")
        );
        running.bridge.send(Job::CancelPlaylistImport);
        running.wait(|s| s.playlist_preview.is_none() && !s.import_busy);
        running.bridge.send(Job::ImportPlaylistFile(path));
        running.wait(|s| s.playlist_preview.is_some());
        running.bridge.send(Job::CommitPlaylistImport);
        running.wait(|s| s.lists.iter().any(|list| list.id == "desktop-import"));
        running.bridge.shutdown();
        running.backend.shutdown(Duration::from_secs(15)).unwrap();
        let restarted = BackendHandle::start(lx_gpui_player::backend::BackendConfig::headless(
            running.directory.path().join("profile"),
        ))
        .unwrap();
        let BackendQueryResult::ListTracks(page) = restarted
            .query(BackendQuery::ListTracks {
                list_id: "desktop-import".into(),
                offset: 0,
                limit: 10,
            })
            .unwrap()
        else {
            panic!("Expected list tracks");
        };
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].title, "原创测试曲");
        restarted.shutdown(Duration::from_secs(15)).unwrap();
    }

    struct PlaylistFixture;
    impl HttpTransport for PlaylistFixture {
        fn execute(
            &self,
            request: HttpRequest,
            context: &ProviderContext,
        ) -> Result<lx_gpui_player::platforms::HttpResponse, lx_gpui_player::platforms::ProviderError>
        {
            context.check()?;
            let url = reqwest::Url::parse(&request.url).unwrap();
            let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
            let page: usize = query.get("pn").unwrap().parse().unwrap();
            let limit: usize = query.get("rn").unwrap().parse().unwrap();
            let total = 1003;
            let songs = (page * limit..((page + 1) * limit).min(total)).map(|i| serde_json::json!({
                "id":i + 1,"name":format!("Original {i}"),"artist":"Fixture","album":"Album","albumid":10,"duration":200,
                "N_MINFO":"level:standard,bitrate:128,format:mp3,size:3m"
            })).collect::<Vec<_>>();
            let body = serde_json::json!({"result":"ok","title":"多页歌单","pic":"","uname":"Fixture","info":"Original metadata fixture","total":total,"musiclist":songs});
            Ok(lx_gpui_player::platforms::HttpResponse {
                status: 200,
                headers: Default::default(),
                bytes: serde_json::to_vec(&body).unwrap(),
            })
        }
    }
    #[test]
    fn desktop_link_import_fetches_all_pages_without_starting_playback() {
        let running = Running::new(Some(Arc::new(PlaylistFixture)));
        running.bridge.send(Job::ImportPlaylistUrl {
            platform: Platform::Kw,
            input: "12345".into(),
        });
        running.wait(|s| s.lists.iter().any(|list| list.name == "多页歌单"));
        let id = running
            .bridge
            .store()
            .lists
            .iter()
            .find(|list| list.name == "多页歌单")
            .unwrap()
            .id
            .clone();
        let BackendQueryResult::ListTracks(page) = running
            .backend
            .query(BackendQuery::ListTracks {
                list_id: id,
                offset: 1000,
                limit: 10,
            })
            .unwrap()
        else {
            panic!("Expected list tracks");
        };
        assert_eq!(page.total, 1003);
        assert_eq!(page.items.len(), 3);
        assert!(!running.bridge.store().import_busy);
        assert!(running.backend.snapshot().now_playing.is_none());
    }
}
