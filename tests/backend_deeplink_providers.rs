use lx_gpui_player::{
    audio::PlaybackStatus,
    backend::{
        BackendCommand, BackendConfig, BackendEvent, BackendHandle, BackendQuery,
        BackendQueryResult, CommandEnvelope, DeepLinkOperationPage, DeepLinkPageMetadata,
        ErrorKind, QueueSnapshot,
    },
    domain::lists::{ListAction, TEMP_LIST},
    platforms::{
        HttpRequest, HttpResponse, HttpTransport, Platform, ProviderContext, ProviderError,
        ProviderRegistry,
    },
    store::ProjectStore,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn kw_song(id: usize, opaque: usize) -> Value {
    json!({"MUSICRID":format!("MUSIC_{id}"),"SONGNAME":"Original Tone","ARTIST":"Artist A&Artist B","ALBUM":"Original Album","ALBUMID":"10","DURATION":"200","N_MINFO":"level:standard,bitrate:128,format:mp3,size:3m","opaque":"x".repeat(opaque)})
}
fn list_song(id: usize) -> Value {
    json!({"id":id,"name":format!("Collection Tone {id}"),"artist":"Fixture Artist","album":"Fixture Album","albumid":10,"duration":200,"N_MINFO":"level:standard,bitrate:128,format:mp3,size:3m"})
}
fn search_replies() -> BTreeMap<Platform, Value> {
    BTreeMap::from([
        (
            Platform::Kw,
            json!({"TOTAL":"1000","SHOW":"1","abslist":[kw_song(101,0)]}),
        ),
        (
            Platform::Kg,
            json!({"error_code":0,"data":{"total":1000,"lists":[{"Audioid":201,"MixSongID":1201,"OriSongName":"Original Tone","Suffix":"","Singers":[{"name":"Artist A"},{"name":"Artist B"}],"AlbumName":"Original Album","AlbumID":10,"Duration":200,"FileHash":"original-hash-201","FileSize":1024,"HQFileHash":"hq-hash-201","HQFileSize":2048,"SQFileHash":"lossless-hash-201","SQFileSize":4096}]}}),
        ),
        (
            Platform::Tx,
            json!({"code":0,"music.search.SearchCgiService":{"code":0,"data":{"meta":{"sum":1000},"body":{"song":{"list":[{"id":301,"mid":"original-mid","title":"Original Tone","interval":200,"album":{"mid":"album-mid","name":"Original Album"},"singer":[{"name":"Artist A"},{"name":"Artist B"}],"file":{"media_mid":"original-media-mid","size_128mp3":1024,"size_flac":4096}}]}}}}}),
        ),
        (
            Platform::Wy,
            json!({"code":200,"data":{"totalCount":1000,"resources":[{"baseInfo":{"simpleSongData":{"id":401,"name":"Original Tone","ar":[{"name":"Artist A"},{"name":"Artist B"}],"al":{"id":10,"name":"Original Album"},"dt":200000,"l":{"size":1024},"sq":{"size":4096},"privilege":{"id":401,"maxbr":999000,"maxBrLevel":"hires"}}}}]}}),
        ),
        (
            Platform::Mg,
            json!({"code":"000000","songResultData":{"totalCount":"1000","resultList":[[{"songId":"original-migu-id","copyrightId":"original-copyright-id","songName":"Original Tone","name":"Original Tone","artists":[{"name":"Artist A"},{"name":"Artist B"}],"singerList":[{"name":"Artist A"},{"name":"Artist B"}],"album":"Original Album","albumId":"10","duration":200,"length":"00:03:20","audioFormats":[{"formatType":"PQ","asize":1024},{"formatType":"SQ","size":4096}],"newRateFormats":[{"formatType":"PQ","size":1024},{"formatType":"SQ","size":4096}]}]]}}),
        ),
    ])
}

struct MetadataFixture {
    replies: Mutex<BTreeMap<Platform, Value>>,
    requests: Mutex<Vec<HttpRequest>>,
    blocked: AtomicBool,
    block_second_page: AtomicBool,
    repeat_second_page: AtomicBool,
    active: AtomicUsize,
    maximum_active: AtomicUsize,
    playlist_total: AtomicUsize,
}
impl MetadataFixture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            replies: Mutex::new(search_replies()),
            requests: Mutex::new(Vec::new()),
            blocked: AtomicBool::new(false),
            block_second_page: AtomicBool::new(false),
            repeat_second_page: AtomicBool::new(false),
            active: AtomicUsize::new(0),
            maximum_active: AtomicUsize::new(0),
            playlist_total: AtomicUsize::new(1003),
        })
    }
}
impl HttpTransport for MetadataFixture {
    fn execute(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.maximum_active.fetch_max(active, Ordering::AcqRel);
        struct Guard<'a>(&'a AtomicUsize);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::AcqRel);
            }
        }
        let _guard = Guard(&self.active);
        let url = reqwest::Url::parse(&request.url).unwrap();
        let host = url.host_str().unwrap_or_default();
        let platform = if host.contains("kuwo") {
            Platform::Kw
        } else if host.contains("kugou") {
            Platform::Kg
        } else if host.contains("qq.com") {
            Platform::Tx
        } else if host.contains("163.com") {
            Platform::Wy
        } else if host.contains("migu") {
            Platform::Mg
        } else {
            return Err(ProviderError::invalid("Unexpected native metadata host"));
        };
        let parameters: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
        let playlist = parameters.get("op").is_some_and(|op| op == "getlistinfo");
        let page = parameters
            .get("pn")
            .and_then(|page| page.parse::<usize>().ok())
            .unwrap_or(0);
        self.requests.lock().unwrap().push(request);
        while self.blocked.load(Ordering::Acquire)
            || (playlist && page > 0 && self.block_second_page.load(Ordering::Acquire))
        {
            context.check()?;
            thread::sleep(Duration::from_millis(2));
        }
        context.check()?;
        let body = if playlist {
            let total = self.playlist_total.load(Ordering::Acquire);
            let limit = parameters
                .get("rn")
                .and_then(|value| value.parse::<usize>().ok())
                .unwrap_or(1000);
            let start = if page > 0 && self.repeat_second_page.load(Ordering::Acquire) {
                0
            } else {
                page * limit
            };
            let tracks = (start..(start + limit).min(total))
                .map(|index| list_song(index + 1))
                .collect::<Vec<_>>();
            json!({"result":"ok","title":"Actual Collection","pic":"","uname":"Fixture","info":"Actual provider details","total":total,"musiclist":tracks})
        } else {
            self.replies.lock().unwrap().get(&platform).unwrap().clone()
        };
        Ok(HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            bytes: serde_json::to_vec(&body).unwrap(),
        })
    }
}
struct Running {
    handle: BackendHandle,
    directory: tempfile::TempDir,
    fixture: Arc<MetadataFixture>,
}
impl Running {
    fn new(fixture: Arc<MetadataFixture>, timeout: Duration) -> Self {
        let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let mut config = BackendConfig::headless(directory.path().into());
        config.provider_registry = Some(Arc::new(ProviderRegistry::new(fixture.clone())));
        config.provider_timeout = timeout;
        Self {
            handle: BackendHandle::start(config).unwrap(),
            directory,
            fixture,
        }
    }
    fn execute(&self, id: &str, command: BackendCommand) {
        self.handle
            .execute(CommandEnvelope {
                request_id: id.into(),
                origin: "deeplink-fixture".into(),
                expected_revision: None,
                command,
            })
            .unwrap();
    }
    fn dispatch(&self, id: &str, kind: &str, action: &str, data: Value) {
        self.execute(
            id,
            BackendCommand::DispatchDeepLink {
                url: format!(
                    "lxmusic://{kind}/{action}?data={}",
                    lx_gpui_player::platforms::encode(&serde_json::to_string(&data).unwrap())
                ),
            },
        );
    }
    fn page(&self, id: &str, offset: usize, limit: usize) -> DeepLinkOperationPage {
        let BackendQueryResult::DeepLinkResult(page) = self
            .handle
            .query(BackendQuery::DeepLinkResult {
                operation_id: id.into(),
                offset,
                limit,
            })
            .unwrap()
        else {
            panic!("DeepLinkResult")
        };
        *page
    }
    fn finish(&self, id: &str) -> DeepLinkOperationPage {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            let page = self.page(id, 0, 100);
            let state = serde_json::to_value(page.state).unwrap();
            if state != "running" {
                return page;
            }
            assert!(
                Instant::now() < deadline,
                "Deep-link operation did not finish"
            );
            thread::sleep(Duration::from_millis(5));
        }
    }
    fn temp_len(&self) -> usize {
        let BackendQueryResult::ListTracks(page) = self
            .handle
            .query(BackendQuery::ListTracks {
                list_id: TEMP_LIST.into(),
                offset: 0,
                limit: 1000,
            })
            .unwrap()
        else {
            panic!("TEMP")
        };
        page.total
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.fixture.blocked.store(false, Ordering::Release);
        self.fixture
            .block_second_page
            .store(false, Ordering::Release);
        self.handle.shutdown(Duration::from_secs(5)).unwrap();
    }
}
fn wait(condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "Expected provider stage was not observed"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

fn queue(running: &Running) -> QueueSnapshot {
    let BackendQueryResult::Queue(queue) = running
        .handle
        .query(BackendQuery::Queue {
            offset: 0,
            limit: 10,
        })
        .unwrap()
    else {
        panic!("Queue");
    };
    *queue
}

#[test]
fn all_five_native_search_codecs_complete_in_order_and_queries_page_actual_rows() {
    let fixture = MetadataFixture::new();
    let running = Running::new(fixture.clone(), Duration::from_secs(3));
    let revision = running.handle.snapshot().revision;
    let cursor = running.handle.event_cursor();
    running.dispatch(
        "all-five",
        "music",
        "search",
        json!({"keywords":"Original Tone","source":"all"}),
    );
    let page = running.finish("all-five");
    assert_eq!(serde_json::to_value(page.state).unwrap(), "completed");
    assert_eq!(page.total, 5);
    assert_eq!(
        page.tracks
            .iter()
            .map(|track| track.platform.source.as_str())
            .collect::<Vec<_>>(),
        ["kw", "kg", "tx", "wy", "mg"]
    );
    assert_eq!(
        running
            .page("all-five", 2, 2)
            .tracks
            .iter()
            .map(|track| track.platform.source.as_str())
            .collect::<Vec<_>>(),
        ["tx", "wy"]
    );
    let Some(DeepLinkPageMetadata::Search { providers, .. }) = page.metadata else {
        panic!("Search metadata")
    };
    assert_eq!(providers.len(), 5);
    assert!(providers.iter().all(|provider| provider.fetched == 1
        && provider.reported_total == Some(1000)
        && provider.error.is_none()));
    assert_eq!(fixture.requests.lock().unwrap().len(), 5);
    assert_eq!(fixture.maximum_active.load(Ordering::Acquire), 1);
    assert_eq!(running.handle.snapshot().revision, revision);
    assert_eq!(running.temp_len(), 0);
    let events = running.handle.events_after(cursor).unwrap();
    assert!(events.iter().any(|event|matches!(&event.payload,BackendEvent::OperationFinished{operation_id,success:true,..}if operation_id=="all-five")));
}

#[test]
fn source_none_keeps_router_intention_while_native_default_query_fetches_kw() {
    let fixture = MetadataFixture::new();
    let running = Running::new(fixture.clone(), Duration::from_secs(3));
    running.dispatch(
        "default-search",
        "music",
        "search",
        json!({"keywords":"Original Tone"}),
    );
    let page = running.finish("default-search");
    assert_eq!(page.total, 1);
    assert_eq!(page.tracks[0].platform.source, "kw");
    let Some(DeepLinkPageMetadata::Search {
        requested_source,
        native_default_query,
        ..
    }) = page.metadata
    else {
        panic!("Search metadata")
    };
    assert!(requested_source.is_none() && native_default_query);
    assert_eq!(fixture.requests.lock().unwrap().len(), 1);
}

#[test]
fn search_play_uses_provider_first_match_and_stable_priority_without_granting_source_or_url() {
    let fixture = MetadataFixture::new();
    fixture
        .replies
        .lock()
        .unwrap()
        .get_mut(&Platform::Kw)
        .unwrap()["abslist"][0]["ARTIST"] = "Artist A".into();
    let running = Running::new(fixture, Duration::from_secs(3));
    assert!(running.handle.snapshot().approved_audio_identity.is_none());
    assert!(queue(&running).current.is_none());
    running.dispatch("search-play","music","searchPlay",json!({"name":"Original Tone","singer":"Artist A&Artist B","albumName":"Original Album","interval":"03:20","playLater":true}));
    let page = running.finish("search-play");
    assert_eq!(page.tracks[0].platform.source, "kg");
    assert!(page.effect.temporary_list_applied && page.effect.queue_enqueued);
    assert_eq!(running.temp_len(), 1);
    assert!(page.effect.playback_requested);
    assert_eq!(
        page.effect.playback_operation_id.as_deref(),
        Some(lx_gpui_player::domain::music::stable_id("deeplink-play", b"search-play").as_str())
    );
    let data = ProjectStore::open(running.directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert!(data.sources.is_empty() && data.selected.is_none() && data.local_files.is_empty());
    let permitted = running.handle.snapshot();
    let identity = permitted.approved_audio_identity.as_ref().unwrap();
    assert_eq!(identity.url_sha256.len(), 64);
    assert!(
        identity
            .url_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    );
    let mut scope = serde_json::to_value(identity).unwrap();
    scope.as_object_mut().unwrap().remove("url_sha256");
    assert_eq!(
        scope,
        json!({
            "track_id":"tone-1", "platform":"local", "actual_quality":"local",
            "source_sha256":"", "source_policy_version":0, "source_epoch":0,
            "approval_id":"catalog-asset:tone-1", "approval_epoch":0,
            "media_context_sha256":format!("{:x}", Sha256::digest(
                br#"["original-catalog-v1","tone-1","wav",45.0]"#
            ))
        })
    );
    let current = queue(&running).current.unwrap();
    assert_eq!(current.list_id, "catalog");
    assert_eq!(current.track_id, "tone-1");
    assert!(!current.temporary);
    assert!(permitted.pending_playback_url.is_none());
    running.dispatch("interval-prefix","music","searchPlay",json!({"name":"Original Tone","singer":"Artist A&Artist B","albumName":"Original Album","interval":"03:00suffix","playLater":true}));
    let unmatched = running.finish("interval-prefix");
    assert_eq!(unmatched.total, 0);
    assert!(!unmatched.effect.temporary_list_applied);
    assert_eq!(running.temp_len(), 1);
}

#[test]
fn search_play_later_preserves_the_paused_current_catalog_identity_and_position() {
    let fixture = MetadataFixture::new();
    let running = Running::new(fixture.clone(), Duration::from_secs(3));
    running.execute(
        "existing-catalog-play",
        BackendCommand::Play {
            id: "tone-2".into(),
        },
    );
    wait(|| {
        let state = running.handle.snapshot();
        state.playback.status == PlaybackStatus::Playing
            && state.playback.has_presented_media
            && state.playback.position_seconds > 0.0
    });
    running.execute("existing-catalog-pause", BackendCommand::Pause);
    wait(|| running.handle.snapshot().playback.status == PlaybackStatus::Paused);
    let before = running.handle.snapshot();
    let before_queue = queue(&running);
    let before_store = ProjectStore::open(running.directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    let identity = before.approved_audio_identity.as_ref().unwrap();
    assert_eq!(identity.track_id, "tone-2");
    assert_eq!(identity.platform, "local");
    assert_eq!(identity.approval_id, "catalog-asset:tone-2");
    assert!(before.playback.position_seconds > 0.0);
    assert_eq!(before_queue.current.as_ref().unwrap().track_id, "tone-2");
    running.dispatch(
        "later-existing-current",
        "music",
        "searchPlay",
        json!({"name":"Original Tone","singer":"Artist A&Artist B",
            "albumName":"Original Album","interval":"03:20","playLater":true}),
    );
    let page = running.finish("later-existing-current");
    assert_eq!(page.tracks[0].platform.source, "kw");
    assert!(page.effect.temporary_list_applied && page.effect.queue_enqueued);
    assert!(!page.effect.playback_requested);
    assert!(page.effect.playback_operation_id.is_none());
    assert!(page.effect.playback_error.is_none());
    let after = running.handle.snapshot();
    let after_queue = queue(&running);
    assert_eq!(after.playback.status, PlaybackStatus::Paused);
    assert_eq!(after.playback.generation, before.playback.generation);
    assert_eq!(after.playback.seek_seq, before.playback.seek_seq);
    assert_eq!(
        after.playback.position_seconds,
        before.playback.position_seconds
    );
    assert_eq!(after.playback.track_id, before.playback.track_id);
    assert_eq!(after.now_playing, before.now_playing);
    assert_eq!(
        serde_json::to_value(&after.approved_audio_identity).unwrap(),
        serde_json::to_value(&before.approved_audio_identity).unwrap()
    );
    assert_eq!(after_queue.current, before_queue.current);
    assert_eq!(after_queue.position_ms, before_queue.position_ms);
    assert_eq!(after_queue.origin_list, before_queue.origin_list);
    assert_eq!(
        after_queue.temporary_total,
        before_queue.temporary_total + 1
    );
    assert!(after.pending_playback_url.is_none());
    let after_store = ProjectStore::open(running.directory.path().into())
        .unwrap()
        .load()
        .unwrap();
    assert_eq!(
        serde_json::to_value(after_store).unwrap(),
        serde_json::to_value(before_store).unwrap()
    );
    assert_eq!(fixture.requests.lock().unwrap().len(), 5);
}

#[test]
fn native_playlist_url_resolution_loads_page_two_and_open_has_no_queue_effect() {
    let fixture = MetadataFixture::new();
    let running = Running::new(fixture.clone(), Duration::from_secs(3));
    running.dispatch(
        "details",
        "songlist",
        "open",
        json!({"source":"kw","url":"https://www.kuwo.cn/playlist_detail/42"}),
    );
    let page = running.finish("details");
    assert_eq!(page.total, 1003);
    assert!(!page.effect.temporary_list_applied);
    assert_eq!(running.temp_len(), 0);
    let tail = running.page("details", 1000, 100);
    assert_eq!(tail.tracks.len(), 3);
    assert!(!tail.has_more);
    assert_eq!(tail.next_offset, 1003);
    let Some(DeepLinkPageMetadata::Songlist {
        info,
        pages_loaded,
        complete,
        selected_index,
        ..
    }) = page.metadata
    else {
        panic!("Songlist")
    };
    assert_eq!(info.id, "42");
    assert_eq!(pages_loaded, 2);
    assert!(complete && selected_index.is_none());
    assert!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|request| request.url.contains("pid=42"))
    );
    fixture.repeat_second_page.store(true, Ordering::Release);
    running.dispatch(
        "repeated-page",
        "songlist",
        "open",
        json!({"source":"kw","id":42}),
    );
    let repeated = running.finish("repeated-page");
    assert!(repeated.error.unwrap().message.contains("repeated"));
    assert_eq!(running.temp_len(), 0);
}

#[test]
fn playlist_play_waits_for_requested_one_based_index_before_persisting_all_tracks() {
    let fixture = MetadataFixture::new();
    fixture.block_second_page.store(true, Ordering::Release);
    let running = Running::new(fixture.clone(), Duration::from_secs(5));
    running.dispatch(
        "later-index",
        "songlist",
        "play",
        json!({"source":"kw","id":42,"index":1001}),
    );
    wait(|| fixture.requests.lock().unwrap().len() >= 2);
    assert_eq!(running.temp_len(), 0);
    fixture.block_second_page.store(false, Ordering::Release);
    let page = running.finish("later-index");
    assert_eq!(running.temp_len(), 1003);
    assert!(page.effect.temporary_list_applied && !page.effect.early_page_applied);
    let Some(DeepLinkPageMetadata::Songlist { selected_index, .. }) = page.metadata else {
        panic!("Songlist")
    };
    assert_eq!(selected_index, Some(1000));
    assert!(running.handle.snapshot().approved_audio_identity.is_none());
}

#[test]
fn temp_aba_change_fences_late_search_play_but_cancel_and_deadline_still_close_workers() {
    let fixture = MetadataFixture::new();
    fixture.blocked.store(true, Ordering::Release);
    let running = Running::new(fixture.clone(), Duration::from_secs(3));
    running.dispatch(
        "stale",
        "music",
        "searchPlay",
        json!({"name":"Original Tone","singer":"Artist A&Artist B","playLater":true}),
    );
    wait(|| !fixture.requests.lock().unwrap().is_empty());
    running.execute(
        "temp-aba",
        BackendCommand::ListAction {
            action: Box::new(ListAction::MusicOverwrite {
                list_id: TEMP_LIST.into(),
                tracks: vec![],
            }),
        },
    );
    fixture.blocked.store(false, Ordering::Release);
    let stale = running.finish("stale");
    assert_eq!(stale.error.unwrap().kind, ErrorKind::Conflict);
    assert_eq!(running.temp_len(), 0);
    fixture.blocked.store(true, Ordering::Release);
    let before = fixture.requests.lock().unwrap().len();
    running.dispatch(
        "cancelled",
        "music",
        "search",
        json!({"keywords":"Original Tone","source":"all"}),
    );
    wait(|| fixture.requests.lock().unwrap().len() > before);
    running.execute(
        "cancel-request",
        BackendCommand::CancelOperation {
            operation_id: "cancelled".into(),
        },
    );
    let cancelled = running.finish("cancelled");
    assert_eq!(cancelled.error.unwrap().kind, ErrorKind::Cancelled);
    wait(|| fixture.active.load(Ordering::Acquire) == 0);
    fixture.blocked.store(false, Ordering::Release);
    let fixture = MetadataFixture::new();
    fixture.blocked.store(true, Ordering::Release);
    let deadline = Running::new(fixture.clone(), Duration::from_millis(60));
    deadline.dispatch(
        "timeout",
        "music",
        "search",
        json!({"keywords":"Original Tone","source":"kw"}),
    );
    assert_eq!(
        deadline.finish("timeout").error.unwrap().kind,
        ErrorKind::Timeout
    );
    wait(|| fixture.active.load(Ordering::Acquire) == 0);
}

#[test]
fn owned_early_playlist_can_finish_after_an_unrelated_user_volume_change() {
    let fixture = MetadataFixture::new();
    fixture.block_second_page.store(true, Ordering::Release);
    let running = Running::new(fixture.clone(), Duration::from_secs(5));
    running.dispatch(
        "early-page",
        "songlist",
        "play",
        json!({"source":"kw","id":42,"index":1}),
    );
    wait(|| fixture.requests.lock().unwrap().len() >= 2 && running.temp_len() == 1000);
    let initial = running.page("early-page", 0, 3);
    assert!(initial.effect.early_page_applied);
    assert_eq!(initial.total, 1000);
    running.execute("unrelated-volume", BackendCommand::Volume { volume: 0.37 });
    fixture.block_second_page.store(false, Ordering::Release);
    let final_page = running.finish("early-page");
    assert_eq!(serde_json::to_value(final_page.state).unwrap(), "completed");
    assert_eq!(running.temp_len(), 1003);
    assert_eq!(final_page.total, 1003);
    assert!(final_page.effect.early_page_applied);
    assert_eq!(
        ProjectStore::open(running.directory.path().into())
            .unwrap()
            .load()
            .unwrap()
            .volume,
        0.37
    );
}

#[test]
fn query_byte_budget_returns_real_next_cursor_without_shrinking_cached_dataset() {
    let fixture = MetadataFixture::new();
    fixture.replies.lock().unwrap().insert(Platform::Kw,json!({"TOTAL":"25","SHOW":"1","abslist":(1..=25).map(|id|kw_song(id,90*1024)).collect::<Vec<_>>()}));
    let running = Running::new(fixture, Duration::from_secs(5));
    running.dispatch(
        "large-pages",
        "music",
        "search",
        json!({"keywords":"Original Tone","source":"kw"}),
    );
    let page = running.finish("large-pages");
    assert_eq!(page.total, 25);
    assert!(page.has_more && page.tracks.len() < 25);
    assert_eq!(page.next_offset, page.tracks.len());
    assert!(serde_json::to_vec(&page).unwrap().len() <= 2 * 1024 * 1024);
    let mut count = page.tracks.len();
    let mut offset = page.next_offset;
    while offset < 25 {
        let next = running.page("large-pages", offset, 1000);
        assert_eq!(next.total, 25);
        assert_eq!(next.offset, offset);
        assert!(!next.tracks.is_empty());
        assert!(serde_json::to_vec(&next).unwrap().len() <= 2 * 1024 * 1024);
        count += next.tracks.len();
        offset = next.next_offset;
    }
    assert_eq!(count, 25);
    assert!(
        running
            .handle
            .query(BackendQuery::DeepLinkResult {
                operation_id: "large-pages".into(),
                offset: 0,
                limit: 1001
            })
            .is_err()
    );
}

#[test]
fn oversized_single_record_is_a_page_quota_error_and_playlist_track_cap_is_explicit() {
    let fixture = MetadataFixture::new();
    fixture.replies.lock().unwrap().insert(
        Platform::Kw,
        json!({"TOTAL":"1","SHOW":"1","abslist":[kw_song(1,1100*1024)]}),
    );
    let running = Running::new(fixture, Duration::from_secs(5));
    running.dispatch(
        "single-large",
        "music",
        "search",
        json!({"keywords":"Original Tone","source":"kw"}),
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match running.handle.query(BackendQuery::DeepLinkResult {
            operation_id: "single-large".into(),
            offset: 0,
            limit: 1,
        }) {
            Err(error) => {
                assert_eq!(error.kind, ErrorKind::Busy);
                break;
            }
            Ok(BackendQueryResult::DeepLinkResult(page))
                if serde_json::to_value(page.state).unwrap() == "running" => {}
            other => panic!("Expected oversized record quota error: {other:?}"),
        };
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    let fixture = MetadataFixture::new();
    fixture.playlist_total.store(50001, Ordering::Release);
    let running = Running::new(fixture, Duration::from_secs(3));
    running.dispatch(
        "too-many",
        "songlist",
        "play",
        json!({"source":"kw","id":42}),
    );
    let result = running.finish("too-many");
    assert!(result.error.is_some());
    assert_eq!(running.temp_len(), 0);
    assert!(!result.effect.temporary_list_applied);
}
