use lx_gpui_player::{
    domain::{
        media::{MatchReason, track_record_fingerprint},
        music::{LxMusicDtoVersion, TrackRecord},
    },
    platforms::{
        matching::{
            MATCHING_ALGORITHM_VERSION, MAX_MATCH_JSON_BYTES, MatchingService, normalize,
            normalize_text, rank_candidates,
        },
        *,
    },
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

fn record(
    source: &str,
    id: &str,
    title: &str,
    singer: &str,
    album: &str,
    duration: Option<u64>,
) -> TrackRecord {
    let mut track = TrackRecord::from_lx_dto(json!({"name":title,"singer":singer,"albumName":album,"source":source,"songmid":id,"hash":format!("hash-{id}"),"types":[{"type":"128k"},{"type":"flac"}],"_types":{"128k":{},"flac":{}}}), LxMusicDtoVersion::Legacy).unwrap();
    track.duration_ms = duration;
    track
}
fn original() -> TrackRecord {
    record(
        "kw",
        "original",
        "Original Tone",
        "Artist A、Artist B",
        "Original Album",
        Some(200000),
    )
}

#[test]
fn normalization_has_explicit_punctuation_artist_order_and_unknown_duration_boundaries() {
    assert_eq!(normalize_text(" A.'&,，\"、()（）`~-<>|/][!！ B\t\n"), "ab");
    let mut track = original();
    track.artists = vec![" Artist B & Artist A；ARTIST A / Artist B ".into()];
    track.duration_ms = Some(0);
    let normalized = normalize(&track);
    assert_eq!(normalized.artists, ["artista", "artistb"]);
    assert_eq!(normalized.title, "originaltone");
    assert_eq!(normalized.album, "originalalbum");
    assert_eq!(normalized.duration_ms, None);
}

#[test]
fn scoring_preserves_the_nine_ordered_lx_tiers_and_deterministic_ties() {
    let original = original();
    let variants = [
        (
            "10",
            "Original Tone",
            "Artist B、Artist A",
            "Different",
            200000,
            10000,
        ),
        (
            "09",
            "Original Tone",
            "Artist A、Artist B",
            "Original Album",
            201000,
            9000,
        ),
        (
            "08",
            "Original Tone",
            "Artist A、Artist B",
            "Different",
            201000,
            8000,
        ),
        ("07", "Original Tone", "Artist A", "Different", 200000, 7000),
        (
            "06",
            "Original Tone extra",
            "Artist A、Artist B",
            "Different",
            200000,
            6000,
        ),
        (
            "05",
            "Original Tone extra",
            "Artist A",
            "Original Album",
            200000,
            5000,
        ),
        ("04", "Original Tone", "Artist A", "Different", 201000, 4000),
        (
            "03",
            "Original Tone extra",
            "Artist A、Artist B",
            "Different",
            201000,
            3000,
        ),
        (
            "02",
            "Original Tone extra",
            "Artist A",
            "Original Album",
            201000,
            2000,
        ),
    ];
    let mut records: Vec<_> = variants
        .iter()
        .rev()
        .map(|(id, title, artist, album, duration, _)| {
            record("tx", id, title, artist, album, Some(*duration))
        })
        .collect();
    records.push(records[0].clone());
    let ranked = rank_candidates(&original, &records).unwrap();
    assert_eq!(
        ranked
            .iter()
            .map(|candidate| candidate.score)
            .collect::<Vec<_>>(),
        variants.iter().map(|variant| variant.5).collect::<Vec<_>>()
    );
    assert_eq!(ranked.len(), 9);
    let mut reversed = records;
    reversed.reverse();
    assert_eq!(ranked, rank_candidates(&original, &reversed).unwrap());
    assert!(ranked[0].reasons.contains(&MatchReason::ExactArtists));
    assert!(ranked[0].reasons.contains(&MatchReason::DurationExact));
    assert!(
        ranked
            .iter()
            .all(|candidate| candidate.actual_quality.is_none())
    );
    let mut without_album = original;
    without_album.album.clear();
    let lowest = record(
        "tx",
        "lowest",
        "Original Tone extra",
        "Artist A",
        "Other",
        Some(201000),
    );
    assert_eq!(
        rank_candidates(&without_album, &[lowest]).unwrap()[0].score,
        1000
    );
}

#[test]
fn duration_uses_the_inclusive_five_second_boundary_and_never_claims_unknown_is_exact() {
    let original = original();
    let candidates = [
        record(
            "tx",
            "upper",
            "Original Tone",
            "Artist A、Artist B",
            "Original Album",
            Some(205000),
        ),
        record(
            "tx",
            "lower",
            "Original Tone",
            "Artist A、Artist B",
            "Original Album",
            Some(195000),
        ),
        record(
            "tx",
            "outside",
            "Original Tone",
            "Artist A、Artist B",
            "Original Album",
            Some(205001),
        ),
        record(
            "tx",
            "unknown",
            "Original Tone",
            "Artist A、Artist B",
            "Original Album",
            None,
        ),
        record(
            "tx",
            "zero",
            "Original Tone",
            "Artist A、Artist B",
            "Original Album",
            Some(0),
        ),
    ];
    let found = rank_candidates(&original, &candidates).unwrap();
    assert_eq!(found.len(), 4);
    assert!(
        !found
            .iter()
            .any(|candidate| candidate.track.platform.song_id == "outside")
    );
    for candidate in found {
        assert!(!candidate.reasons.contains(&MatchReason::DurationExact));
        assert!(
            candidate.reasons.contains(
                if candidate
                    .track
                    .duration_ms
                    .is_none_or(|duration| duration == 0)
                {
                    &MatchReason::DurationUnknown
                } else {
                    &MatchReason::DurationWithinFiveSeconds
                }
            )
        );
    }
}

#[test]
fn live_remix_instrumental_and_every_other_version_family_are_not_interchangeable() {
    let original = original();
    for suffix in [
        "Live",
        "Remix",
        "伴奏",
        "off vocal",
        "cover",
        "acoustic",
        "demo",
        "sped up",
        "piano",
        "现场",
        "インスト",
        "リミックス",
        "라이브",
    ] {
        let candidate = record(
            "tx",
            "version",
            &format!("Original Tone ({suffix})"),
            "Artist A、Artist B",
            "Original Album",
            Some(200000),
        );
        assert!(
            rank_candidates(&original, std::slice::from_ref(&candidate))
                .unwrap()
                .is_empty(),
            "wrong version accepted: {suffix}"
        );
        let mut same = original.clone();
        same.title = candidate.title.clone();
        assert_eq!(rank_candidates(&same, &[candidate]).unwrap().len(), 1);
    }
    let original = record(
        "kw",
        "vocal",
        "Original Tone (off vocal)",
        "Artist A",
        "",
        Some(200000),
    );
    let same = record(
        "tx",
        "same",
        "Original Tone [OFF-VOCAL]",
        "Artist A",
        "",
        Some(200000),
    );
    assert_eq!(rank_candidates(&original, &[same]).unwrap().len(), 1);
}

#[test]
fn empty_or_substring_only_artists_and_incomplete_platform_mapping_do_not_match() {
    let original = original();
    let mut missing_hash = record(
        "kg",
        "bad-hash",
        "Original Tone",
        "Artist A、Artist B",
        "Original Album",
        Some(200000),
    );
    missing_hash.metadata.remove("hash");
    let mut missing_id = record(
        "tx",
        "bad-id",
        "Original Tone",
        "Artist A、Artist B",
        "Original Album",
        Some(200000),
    );
    missing_id.platform.song_id = Value::Null;
    let candidates = [
        record(
            "tx",
            "no-artist",
            "Original Tone",
            "",
            "Original Album",
            Some(200000),
        ),
        record(
            "tx",
            "different",
            "Original Tone",
            "Artist C",
            "Original Album",
            Some(200000),
        ),
        record(
            "tx",
            "ambiguous",
            "Original Tone extra",
            "Artist A",
            "Different Album",
            Some(200000),
        ),
        record(
            "kw",
            "same-source",
            "Original Tone",
            "Artist A、Artist B",
            "Original Album",
            Some(200000),
        ),
        record(
            "closed",
            "closed",
            "Original Tone",
            "Artist A、Artist B",
            "Original Album",
            Some(200000),
        ),
        missing_hash,
        missing_id,
    ];
    assert!(rank_candidates(&original, &candidates).unwrap().is_empty());
    let ann = record("kw", "ann", "Tone", "Ann", "", Some(1000));
    let joanne = record("tx", "joanne", "Tone", "Joanne", "", Some(1000));
    assert!(rank_candidates(&ann, &[joanne]).unwrap().is_empty());
    let mut no_artist = original;
    no_artist.artists.clear();
    assert!(matches!(
        rank_candidates(&no_artist, &[]),
        Err(ProviderError::InvalidRequest { .. })
    ));
}

#[test]
fn pure_matching_rejects_record_and_json_limits_before_returning_candidates() {
    let original = original();
    let one = record(
        "tx",
        "one",
        "Original Tone",
        "Artist A、Artist B",
        "Original Album",
        Some(200000),
    );
    assert!(matches!(
        rank_candidates(&original, &vec![one.clone(); 101]),
        Err(ProviderError::InvalidRequest { .. })
    ));
    let mut large = one;
    large.metadata.insert(
        "large".into(),
        Value::String("x".repeat(MAX_MATCH_JSON_BYTES)),
    );
    assert_eq!(
        rank_candidates(&original, &[large]),
        Err(ProviderError::ResponseTooLarge {
            limit: MAX_MATCH_JSON_BYTES
        })
    );
}

struct ServerState {
    released: AtomicBool,
    stop: AtomicBool,
    requests: Mutex<Vec<String>>,
    changed: Condvar,
    active: AtomicUsize,
    maximum: AtomicUsize,
    replies: BTreeMap<Platform, Vec<u8>>,
}
struct NativeHttpFixture {
    url: String,
    state: Arc<ServerState>,
    server: Option<thread::JoinHandle<()>>,
}
impl NativeHttpFixture {
    fn new(gated: bool, replies: BTreeMap<Platform, Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(ServerState {
            released: AtomicBool::new(!gated),
            stop: AtomicBool::new(false),
            requests: Mutex::new(Vec::new()),
            changed: Condvar::new(),
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            replies: replies
                .into_iter()
                .map(|(platform, body)| (platform, serde_json::to_vec(&body).unwrap()))
                .collect(),
        });
        let running = state.clone();
        let server = thread::spawn(move || {
            let mut clients = Vec::new();
            while !running.stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let state = running.clone();
                        clients.push(thread::spawn(move || serve(socket, state)));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("metadata fixture accept: {error}"),
                }
            }
            for client in clients {
                client.join().unwrap();
            }
        });
        Self {
            url,
            state,
            server: Some(server),
        }
    }
    fn service(&self) -> (MatchingService, Arc<RewriteTransport>) {
        let transport = Arc::new(RewriteTransport {
            base: self.url.clone(),
            inner: ReqwestTransport::new(TransportConfig {
                proxy: None,
                max_response_bytes: MAX_MATCH_JSON_BYTES,
                retries: 0,
            })
            .unwrap(),
            native_requests: Mutex::new(Vec::new()),
        });
        (
            MatchingService::new(Arc::new(ProviderRegistry::new(transport.clone()))),
            transport,
        )
    }
    fn wait_requests(&self, count: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requests = self.state.requests.lock().unwrap();
        while requests.len() < count {
            assert!(
                Instant::now() < deadline,
                "actual HTTP requests did not reach the fixture"
            );
            requests = self
                .state
                .changed
                .wait_timeout(requests, Duration::from_millis(10))
                .unwrap()
                .0;
        }
    }
}
impl Drop for NativeHttpFixture {
    fn drop(&mut self) {
        self.state.stop.store(true, Ordering::Release);
        self.state.released.store(true, Ordering::Release);
        self.state.changed.notify_all();
        self.server.take().unwrap().join().unwrap();
    }
}
fn serve(mut socket: TcpStream, state: Arc<ServerState>) {
    socket.set_nonblocking(false).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    socket
        .set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut request = Vec::new();
    let mut scratch = [0; 4096];
    let headers_end = loop {
        let Ok(count) = socket.read(&mut scratch) else {
            return;
        };
        if count == 0 {
            return;
        }
        request.extend_from_slice(&scratch[..count]);
        if let Some(end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
            break end;
        }
        assert!(request.len() <= 16 * 1024);
    };
    let header = String::from_utf8_lossy(&request[..headers_end]);
    let length = header
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                .map(|(_, value)| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while request.len() < headers_end + 4 + length {
        let Ok(count) = socket.read(&mut scratch) else {
            return;
        };
        if count == 0 {
            return;
        }
        request.extend_from_slice(&scratch[..count]);
    }
    let target = String::from_utf8_lossy(&request[..headers_end])
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .trim_start_matches('/')
        .to_owned();
    let platform: Platform = serde_json::from_value(json!(target)).unwrap();
    let active = state.active.fetch_add(1, Ordering::AcqRel) + 1;
    state.maximum.fetch_max(active, Ordering::AcqRel);
    let mut requests = state.requests.lock().unwrap();
    requests.push(target);
    state.changed.notify_all();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !state.released.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline);
        requests = state
            .changed
            .wait_timeout(requests, Duration::from_millis(10))
            .unwrap()
            .0;
    }
    drop(requests);
    let body = &state.replies[&platform];
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket
        .write_all(response.as_bytes())
        .and_then(|()| socket.write_all(body));
    state.active.fetch_sub(1, Ordering::AcqRel);
}

struct RewriteTransport {
    base: String,
    inner: ReqwestTransport,
    native_requests: Mutex<Vec<HttpRequest>>,
}
fn eapi_body(request: &HttpRequest) -> Value {
    use aes::{
        Aes128,
        cipher::{Block, BlockDecrypt, KeyInit},
    };
    let body = std::str::from_utf8(&request.body)
        .unwrap()
        .strip_prefix("params=")
        .unwrap();
    let mut bytes: Vec<_> = body
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect();
    let cipher = Aes128::new_from_slice(b"e82ckenh8dichen8").unwrap();
    for block in bytes.as_chunks_mut::<16>().0 {
        cipher.decrypt_block(Block::<Aes128>::from_mut_slice(block));
    }
    bytes.truncate(bytes.len() - *bytes.last().unwrap() as usize);
    let plain = String::from_utf8(bytes).unwrap();
    let parts: Vec<_> = plain.split("-36cd479b6b5-").collect();
    assert_eq!(parts[0], "/api/search/song/list/page");
    serde_json::from_str(parts[1]).unwrap()
}
impl HttpTransport for RewriteTransport {
    fn execute(
        &self,
        request: HttpRequest,
        context: &ProviderContext,
    ) -> Result<HttpResponse, ProviderError> {
        context.check()?;
        let url = reqwest::Url::parse(&request.url).unwrap();
        let query: BTreeMap<_, _> = url
            .query_pairs()
            .map(|(name, value)| (name.into_owned(), value.into_owned()))
            .collect();
        let keyword = "Original Tone Artist A、Artist B";
        let platform = match (url.host_str().unwrap(), url.path(), request.method.as_str()) {
            ("search.kuwo.cn", "/r.s", "GET") => {
                assert_eq!(query["rn"], "25");
                assert_eq!(query["pn"], "0");
                assert_eq!(query["ft"], "music");
                assert_eq!(query["all"], keyword);
                Platform::Kw
            }
            ("songsearch.kugou.com", "/song_search_v2", "GET") => {
                assert_eq!(query["pagesize"], "25");
                assert_eq!(query["page"], "1");
                assert_eq!(query["keyword"], keyword);
                Platform::Kg
            }
            ("u.y.qq.com", "/cgi-bin/musics.fcg", "POST") => {
                let value: Value = serde_json::from_slice(&request.body).unwrap();
                let args = &value["music.search.SearchCgiService"]["param"];
                assert_eq!(args["page_num"], 1);
                assert_eq!(args["num_per_page"], 25);
                assert_eq!(args["query"], keyword);
                Platform::Tx
            }
            ("interface.music.163.com", "/eapi/batch", "POST") => {
                let args = eapi_body(&request);
                assert_eq!(args["offset"], 0);
                assert_eq!(args["limit"], 25);
                assert_eq!(args["keyword"], keyword);
                Platform::Wy
            }
            ("jadeite.migu.cn", "/music_search/v3/search/searchAll", "GET") => {
                assert_eq!(query["pageSize"], "25");
                assert_eq!(query["pageNo"], "1");
                assert_eq!(query["text"], keyword);
                Platform::Mg
            }
            other => panic!("matching attempted a non-metadata search route: {other:?}"),
        };
        self.native_requests.lock().unwrap().push(request.clone());
        let mut rewritten = request;
        rewritten.url = format!("{}/{}", self.base, platform.as_str());
        self.inner.execute(rewritten, context)
    }
}

fn kg_song(id: u64, suffix: &str) -> Value {
    json!({"Audioid":id,"MixSongID":id+1000,"OriSongName":"Original Tone","Suffix":suffix,"Singers":[{"name":"Artist A"},{"name":"Artist B"}],"AlbumName":"Original Album","AlbumID":10,"Duration":200,"FileHash":format!("original-hash-{id}"),"FileSize":1024,"HQFileHash":format!("hq-hash-{id}"),"HQFileSize":2048,"SQFileHash":format!("lossless-hash-{id}"),"SQFileSize":4096})
}
fn kw_song(id: u64) -> Value {
    json!({"MUSICRID":format!("MUSIC_{id}"),"SONGNAME":"Original Tone","ARTIST":"Artist A&Artist B","ALBUM":"Original Album","ALBUMID":"10","DURATION":"200","N_MINFO":"level:lossless,bitrate:2000,format:flac,size:30m;level:standard,bitrate:128,format:mp3,size:3m"})
}
fn replies() -> BTreeMap<Platform, Value> {
    BTreeMap::from([
        (
            Platform::Kw,
            json!({"TOTAL":"1000","SHOW":"1","abslist":[kw_song(101)]}),
        ),
        (
            Platform::Kg,
            json!({"error_code":0,"data":{"total":1000,"lists":[kg_song(201,"")]}}),
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

#[test]
fn real_native_metadata_http_search_preserves_source_identity_quality_hash_and_fingerprint() {
    let fixture = NativeHttpFixture::new(false, replies());
    let (service, transport) = fixture.service();
    let original = original();
    let set = service
        .search(
            &original,
            &[
                Platform::Kw,
                Platform::Kg,
                Platform::Tx,
                Platform::Wy,
                Platform::Mg,
            ],
            &ProviderContext::default(),
        )
        .unwrap();
    assert_eq!(set.algorithm_version, MATCHING_ALGORITHM_VERSION);
    assert_eq!(
        set.original_record_sha256,
        track_record_fingerprint(&original).unwrap()
    );
    assert_eq!(set.candidates.len(), 4, "native matching result: {set:?}");
    assert!(set.failures.is_empty(), "{:?}", set.failures);
    assert_eq!(set.examined_records, 4);
    assert_eq!(set.truncated_records, 0);
    assert_eq!(
        set.searched_platforms,
        [Platform::Tx, Platform::Kg, Platform::Wy, Platform::Mg]
    );
    assert_eq!(transport.native_requests.lock().unwrap().len(), 4);
    assert!(
        set.candidates
            .iter()
            .all(|candidate| candidate.score == 10000
                && candidate.actual_quality.is_none()
                && !candidate.track.platform.qualities.is_empty())
    );
    let kg = set
        .candidates
        .iter()
        .find(|candidate| candidate.track.platform.source == "kg")
        .unwrap();
    assert_eq!(kg.track.platform.song_id, 201);
    assert_eq!(kg.track.metadata["hash"], "original-hash-201");
    assert_eq!(kg.track.to_source_music_info()["hash"], "original-hash-201");
    assert_eq!(service.active_workers(), 0);
    assert_eq!(service.waiting_workers(), 0);
}

#[test]
fn selection_keeps_the_first_valid_record_per_platform_and_bounds_kugou_expansion() {
    let mut bodies = replies();
    let mut group = kg_song(201, "");
    group["Singers"] = json!([{"name":"Artist A"}]);
    group["Grp"] = Value::Array((0..25).map(|index| kg_song(300 + index, "")).collect());
    bodies.insert(
        Platform::Kg,
        json!({"error_code":0,"data":{"total":1,"lists":[group]}}),
    );
    let fixture = NativeHttpFixture::new(false, bodies);
    let (service, _) = fixture.service();
    let set = service
        .search(&original(), &[Platform::Kg], &ProviderContext::default())
        .unwrap();
    assert!(
        set.failures.is_empty(),
        "native grouped matching result: {set:?}"
    );
    assert_eq!(set.candidates.len(), 1);
    assert_eq!(set.candidates[0].track.platform.song_id, 201);
    assert_eq!(
        set.candidates[0].score, 7000,
        "the first valid exact title wins before later stronger artists"
    );
    assert_eq!(set.examined_records, 25);
    assert_eq!(set.truncated_records, 1);
}

#[test]
fn upstream_failures_and_inconsistent_non_grouped_totals_are_reported_without_fabricated_matches() {
    for (total, count) in [(0, 1), (1, 2), (26, 26)] {
        let mut bodies = replies();
        bodies.insert(Platform::Kw,json!({"TOTAL":total.to_string(),"SHOW":"1","abslist":(0..count).map(|id| kw_song(100+id)).collect::<Vec<_>>()}));
        let fixture = NativeHttpFixture::new(false, bodies);
        let (service, _) = fixture.service();
        let mut original = original();
        original.platform.source = "tx".into();
        let set = service
            .search(&original, &[Platform::Kw], &ProviderContext::default())
            .unwrap();
        assert!(set.candidates.is_empty());
        assert_eq!(set.failures.len(), 1);
        assert!(matches!(set.failures[0].error, ProviderError::Parse { .. }));
    }
    let mut bodies = replies();
    bodies.insert(
        Platform::Kg,
        json!({"error_code":1,"error":"original upstream failure"}),
    );
    let fixture = NativeHttpFixture::new(false, bodies);
    let (service, _) = fixture.service();
    let set = service
        .search(
            &original(),
            &[Platform::Kg, Platform::Tx],
            &ProviderContext::default(),
        )
        .unwrap();
    assert_eq!(set.candidates.len(), 1);
    assert_eq!(set.candidates[0].track.platform.source, "tx");
    assert_eq!(set.failures.len(), 1);
}

#[test]
fn shared_admission_limits_actual_http_to_four_and_cancellation_joins_active_and_waiting_workers() {
    let fixture = NativeHttpFixture::new(true, replies());
    let (service, transport) = fixture.service();
    let first_context = ProviderContext::new(Duration::from_secs(5));
    let second_context = ProviderContext::new(Duration::from_secs(5));
    let original = original();
    let allowed = [Platform::Kg, Platform::Tx, Platform::Wy, Platform::Mg];
    thread::scope(|scope| {
        let clone = service.clone();
        let first = scope.spawn(|| service.search(&original, &allowed, &first_context));
        fixture.wait_requests(4);
        let original = &original;
        let allowed = &allowed;
        let context = &second_context;
        let second = scope.spawn(move || clone.search(original, allowed, context));
        let deadline = Instant::now() + Duration::from_secs(5);
        while service.waiting_workers() == 0 {
            assert!(
                Instant::now() < deadline,
                "second search did not reach shared admission"
            );
            thread::yield_now();
        }
        assert_eq!(service.active_workers(), 4);
        assert_eq!(fixture.state.requests.lock().unwrap().len(), 4);
        let stopped = Instant::now();
        first_context.cancel.store(true, Ordering::Release);
        second_context.cancel.store(true, Ordering::Release);
        assert_eq!(first.join().unwrap(), Err(ProviderError::Cancelled));
        assert_eq!(second.join().unwrap(), Err(ProviderError::Cancelled));
        assert!(stopped.elapsed() < Duration::from_secs(1));
    });
    assert_eq!(service.active_workers(), 0);
    assert_eq!(service.waiting_workers(), 0);
    assert_eq!(transport.native_requests.lock().unwrap().len(), 4);
    assert!(fixture.state.maximum.load(Ordering::Acquire) <= 4);
}

#[test]
fn actual_inflight_http_obeys_the_shared_deadline_and_all_workers_are_joined() {
    let fixture = NativeHttpFixture::new(true, replies());
    let (service, _) = fixture.service();
    let context = ProviderContext::new(Duration::from_millis(200));
    let original = original();
    thread::scope(|scope| {
        let search =
            scope.spawn(|| service.search(&original, &[Platform::Tx, Platform::Kg], &context));
        fixture.wait_requests(1);
        assert_eq!(search.join().unwrap(), Err(ProviderError::DeadlineExceeded));
    });
    assert_eq!(service.active_workers(), 0);
    assert_eq!(service.waiting_workers(), 0);
}

#[test]
fn native_http_json_body_over_eight_mib_is_rejected_without_returning_an_unbounded_result() {
    let mut bodies = replies();
    let mut song = kw_song(101);
    song["untrustedLargeField"] = Value::String("x".repeat(MAX_MATCH_JSON_BYTES));
    bodies.insert(
        Platform::Kw,
        json!({"TOTAL":"1","SHOW":"1","abslist":[song]}),
    );
    let fixture = NativeHttpFixture::new(false, bodies);
    let (service, _) = fixture.service();
    let mut original = original();
    original.platform.source = "tx".into();
    assert_eq!(
        service.search(&original, &[Platform::Kw], &ProviderContext::default()),
        Err(ProviderError::ResponseTooLarge {
            limit: MAX_MATCH_JSON_BYTES
        })
    );
    assert_eq!(service.active_workers(), 0);
}

struct ForbiddenTransport;
impl HttpTransport for ForbiddenTransport {
    fn execute(&self, _: HttpRequest, _: &ProviderContext) -> Result<HttpResponse, ProviderError> {
        panic!("invalid matching request must not perform HTTP");
    }
}
#[test]
fn invalid_original_platform_artist_duplicate_platforms_and_pre_cancelled_search_never_start_http()
{
    let service = MatchingService::new(Arc::new(ProviderRegistry::new(Arc::new(
        ForbiddenTransport,
    ))));
    let mut local = original();
    local.platform.source = "local".into();
    assert!(matches!(
        service.search(&local, &[Platform::Kw], &ProviderContext::default()),
        Err(ProviderError::InvalidRequest { .. })
    ));
    let mut empty = original();
    empty.artists.clear();
    assert!(matches!(
        service.search(&empty, &[Platform::Kw], &ProviderContext::default()),
        Err(ProviderError::InvalidRequest { .. })
    ));
    assert!(matches!(
        service.search(
            &original(),
            &[Platform::Tx, Platform::Tx],
            &ProviderContext::default()
        ),
        Err(ProviderError::InvalidRequest { .. })
    ));
    let context = ProviderContext::default();
    context.cancel.store(true, Ordering::Release);
    assert_eq!(
        service.search(&original(), &[Platform::Tx], &context),
        Err(ProviderError::Cancelled)
    );
    assert!(
        service
            .search(&original(), &[Platform::Kw], &ProviderContext::default())
            .unwrap()
            .candidates
            .is_empty()
    );
    assert_eq!(service.active_workers(), 0);
    assert_eq!(service.waiting_workers(), 0);
}
