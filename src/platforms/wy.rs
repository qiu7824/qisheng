//! NetEase operations exported by LX Music v2.12.6.
#[path = "wy_crypto.rs"]
pub mod crypto;
use super::types::*;
use crate::domain::music::{LxMusicDtoVersion, TrackRecord};
use regex::Regex;
use serde_json::{Value, json as j};
use std::{
    collections::HashMap,
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/60.0.3112.90 Safari/537.36";
static YRC_WORD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\((\d+),(\d+),\d+\)").expect("YRC token pattern"));
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct Cursor {
    song: String,
    page: u32,
    limit: u32,
    current: Value,
    previous: Value,
    order: u32,
    offset: u64,
    #[serde(skip)]
    generation: u64,
}
pub struct WyProvider {
    transport: Arc<dyn HttpTransport>,
    cursors: Mutex<HashMap<String, Cursor>>,
    generation: AtomicU64,
}
impl WyProvider {
    pub fn new(transport: Arc<dyn HttpTransport>) -> Self {
        Self {
            transport,
            cursors: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(1),
        }
    }
    fn encrypted(
        &self,
        endpoint: &str,
        body: String,
        referer: Option<&str>,
        cookie: Option<&str>,
        ctx: &ProviderContext,
    ) -> Result<Value, ProviderError> {
        let mut request = HttpRequest::post(endpoint, body.into_bytes())
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("User-Agent", UA)
            .header("origin", "https://music.163.com");
        if let Some(value) = referer {
            request = request.header("Referer", value);
        }
        if let Some(value) = cookie {
            request = request.header("Cookie", value);
        }
        let value = json(self.transport.as_ref(), request, ctx)?;
        if number(&value["code"]) != 200 {
            return Err(ProviderError::upstream(format!(
                "NetEase: {} ({})",
                text(&value["message"]),
                text(&value["code"])
            )));
        }
        Ok(value)
    }
    fn weapi(
        &self,
        path: &str,
        value: Value,
        ctx: &ProviderContext,
    ) -> Result<Value, ProviderError> {
        self.encrypted(
            &format!("https://music.163.com/weapi/{path}"),
            crypto::weapi(&value)?,
            None,
            None,
            ctx,
        )
    }
    fn eapi(
        &self,
        path: &str,
        value: Value,
        ctx: &ProviderContext,
    ) -> Result<Value, ProviderError> {
        self.encrypted(
            "http://interface.music.163.com/eapi/batch",
            crypto::eapi(path, &value),
            None,
            None,
            ctx,
        )
    }
    fn details(
        &self,
        ids: &[String],
        ctx: &ProviderContext,
    ) -> Result<Vec<TrackRecord>, ProviderError> {
        let mut records = Vec::new();
        for group in ids.chunks(1000) {
            let numeric = group
                .iter()
                .map(|id| numeric_id(id))
                .collect::<Result<Vec<_>, _>>()?;
            let c = Value::Array(numeric.iter().map(|id| j!({"id":id})).collect()).to_string();
            let value=self.weapi("v3/song/detail",j!({"c":c,"ids":Value::Array(numeric.iter().copied().map(Value::from).collect()).to_string()}),ctx)?;
            let songs = array(&value["songs"], "songs")?;
            let privileges = value["privileges"].as_array();
            for song in songs {
                let privilege = privileges
                    .and_then(|list| list.iter().find(|p| p["id"] == song["id"]))
                    .unwrap_or(&song["privilege"]);
                records.push(track(song, privilege)?);
            }
        }
        let order: HashMap<_, _> = ids
            .iter()
            .enumerate()
            .map(|(i, id)| (id.as_str(), i))
            .collect();
        records.sort_by_key(|r| {
            order
                .get(text(&r.platform.song_id).as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
        Ok(records)
    }
    fn collection(
        &self,
        raw_id: &str,
        page: u32,
        limit: u32,
        chart: bool,
        ctx: &ProviderContext,
    ) -> Result<Collection, ProviderError> {
        let (page, limit) = page_args(page, limit)?;
        let (raw, token) = raw_id
            .split_once("###")
            .map(|(a, b)| (a, Some(b)))
            .unwrap_or((raw_id, None));
        let id = parse_id(raw)?;
        let value = if chart {
            self.weapi(
                "v3/playlist/detail",
                j!({"id":id.to_string(),"n":100000,"p":1}),
                ctx,
            )?
        } else {
            let payload = j!({"method":"POST","url":"https://music.163.com/api/v3/playlist/detail","params":{"id":id.to_string(),"n":100000,"s":8}});
            self.encrypted(
                "https://music.163.com/api/linux/forward",
                crypto::linuxapi(&payload),
                None,
                Some(&format!("MUSIC_U={}", token.unwrap_or(""))),
                ctx,
            )?
        };
        let playlist = &value["playlist"];
        let ids = array(&playlist["trackIds"], "playlist.trackIds")?;
        let start = (page as usize - 1) * limit as usize;
        let selected: Vec<_> = ids
            .iter()
            .skip(start)
            .take(limit as usize)
            .map(|id| text(&id["id"]))
            .collect();
        let mut tracks = Vec::new();
        if let (Some(songs), Some(privileges)) = (
            playlist["tracks"].as_array(),
            value["privileges"].as_array(),
        ) {
            for id in &selected {
                if let Some(song) = songs.iter().find(|s| text(&s["id"]) == *id) {
                    let p = privileges
                        .iter()
                        .find(|p| p["id"] == song["id"])
                        .unwrap_or(&song["privilege"]);
                    tracks.push(track(song, p)?);
                }
            }
        }
        if tracks.len() != selected.len() {
            tracks = self.details(&selected, ctx)?;
        }
        Ok(Collection {
            info: playlist_info(playlist),
            tracks: Page::new(tracks, ids.len() as u64, page, limit),
        })
    }
    fn comments(
        &self,
        id: &str,
        kind: CommentKind,
        page: u32,
        limit: u32,
        explicit: Option<String>,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        let (page, limit) = page_args(page, limit)?;
        let song = numeric_id(id)?;
        let rid = format!("R_SO_4_{song}");
        if kind == CommentKind::Replies {
            return Err(ProviderError::Unsupported {
                platform: Platform::Wy,
                operation: "comment_replies".into(),
            });
        }
        if kind == CommentKind::Hot {
            let body = j!({"rid":rid,"limit":limit,"offset":(page as u64-1)*limit as u64,"beforeTime":now_ms().to_string()});
            let v = self.encrypted(
                &format!("https://music.163.com/weapi/v1/resource/hotcomments/{rid}"),
                crypto::weapi(&body)?,
                Some("http://music.163.com/"),
                None,
                ctx,
            )?;
            return Ok(ProviderResponse::Comments(Page::new(
                parse_comments(&v["hotComments"])?,
                number(&v["total"]),
                page,
                limit,
            )));
        }
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let cached = self
            .cursors
            .lock()
            .map_err(|_| ProviderError::parse("NetEase cursor lock"))?
            .get(id)
            .cloned();
        let restored = explicit
            .as_deref()
            .and_then(|s| serde_json::from_str::<Cursor>(s).ok());
        if restored.as_ref().is_some_and(|state| state.song != id) {
            return Err(ProviderError::invalid(
                "comment cursor belongs to a different song",
            ));
        }
        let state = restored.as_ref().or(cached.as_ref());
        let (cursor, order, offset) = if page == 1 {
            (j!(now_ms()), 1, 0)
        } else if let Some(s) = state {
            if s.limit != limit {
                return Err(ProviderError::invalid(
                    "comment page size changed; restart at page 1",
                ));
            }
            if page > s.page {
                (
                    s.current.clone(),
                    1,
                    (page as u64 - s.page as u64 - 1) * limit as u64,
                )
            } else if page < s.page {
                (
                    s.current.clone(),
                    0,
                    (s.page as u64 - page as u64 - 1) * limit as u64,
                )
            } else {
                (s.previous.clone(), s.order, s.offset)
            }
        } else if let Some(raw) = explicit {
            (j!(raw), 1, 0)
        } else {
            return Err(ProviderError::invalid(
                "NetEase comments require page 1 or a cursor before later pages",
            ));
        };
        let body = j!({"cursor":cursor,"offset":offset,"orderType":order,"pageNo":page,"pageSize":limit,"rid":rid,"threadId":rid});
        let v = self.encrypted(
            "https://music.163.com/weapi/comment/resource/comments/get",
            crypto::weapi(&body)?,
            Some("http://music.163.com/"),
            None,
            ctx,
        )?;
        let data = &v["data"];
        let next = data
            .get("cursor")
            .cloned()
            .ok_or_else(|| ProviderError::parse("missing comment cursor"))?;
        let s = Cursor {
            song: id.into(),
            page,
            limit,
            current: next,
            previous: cursor,
            order,
            offset,
            generation,
        };
        let mut result = Page::new(
            parse_comments(&data["comments"])?,
            number(&data["totalCount"]),
            page,
            limit,
        );
        result.next_cursor =
            Some(serde_json::to_string(&s).map_err(|e| ProviderError::parse(e.to_string()))?);
        let mut cache = self
            .cursors
            .lock()
            .map_err(|_| ProviderError::parse("NetEase cursor lock"))?;
        if cache.get(id).is_none_or(|old| old.generation < generation) {
            if cache.len() >= 256
                && !cache.contains_key(id)
                && let Some(key) = cache.keys().next().cloned()
            {
                cache.remove(&key);
            }
            cache.insert(id.into(), s);
        }
        Ok(ProviderResponse::Comments(result))
    }
}
impl Provider for WyProvider {
    fn platform(&self) -> Platform {
        Platform::Wy
    }
    fn execute(
        &self,
        request: ProviderRequest,
        ctx: &ProviderContext,
    ) -> Result<ProviderResponse, ProviderError> {
        ctx.check()?;
        match request {
            ProviderRequest::Search {
                keyword,
                page,
                limit,
            } => {
                let (page, limit) = page_args(page, limit)?;
                let v=self.eapi("/api/search/song/list/page",j!({"keyword":keyword,"needCorrect":"1","channel":"typing","offset":(page as u64-1)*limit as u64,"scene":"normal","total":page==1,"limit":limit}),ctx)?;
                let list = array(&v["data"]["resources"], "data.resources")?
                    .iter()
                    .map(|r| {
                        let s = &r["baseInfo"]["simpleSongData"];
                        track(s, &s["privilege"])
                    })
                    .collect::<Result<_, _>>()?;
                Ok(ProviderResponse::Tracks(Page::new(
                    list,
                    number(&v["data"]["totalCount"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::HotSearch => {
                let v = self.eapi(
                    "/api/search/chart/detail",
                    j!({"id":"HOT_SEARCH_SONG#@#"}),
                    ctx,
                )?;
                Ok(ProviderResponse::Keywords(
                    array(&v["data"]["itemList"], "data.itemList")?
                        .iter()
                        .map(|i| text(&i["searchWord"]))
                        .collect(),
                ))
            }
            ProviderRequest::Detail { ids } => {
                let list = self.details(&ids, ctx)?;
                Ok(ProviderResponse::Tracks(Page::new(
                    list.clone(),
                    list.len() as u64,
                    1,
                    ids.len().max(1) as u32,
                )))
            }
            ProviderRequest::Charts => Ok(ProviderResponse::Charts(charts())),
            ProviderRequest::ChartTracks { id, page, limit } => Ok(ProviderResponse::Tracks(
                self.collection(id.trim_start_matches("wy__"), page, limit, true, ctx)?
                    .tracks,
            )),
            ProviderRequest::PlaylistDetail { id, page, limit } => Ok(
                ProviderResponse::Collection(self.collection(&id, page, limit, false, ctx)?),
            ),
            ProviderRequest::PlaylistTags => {
                let v = self.weapi("playlist/catalogue", j!({}), ctx)?;
                let hot = self.weapi("playlist/hottags", j!({}), ctx)?;
                let mut list = Vec::new();
                for item in array(&v["sub"], "sub")? {
                    let group = text(&v["categories"][text(&item["category"])]);
                    list.push(PlaylistTag {
                        id: text(&item["name"]),
                        name: text(&item["name"]),
                        group,
                        extra: item.clone(),
                    });
                }
                for item in array(&hot["tags"], "tags")? {
                    let name = text(&item["playlistTag"]["name"]);
                    list.push(PlaylistTag {
                        id: name.clone(),
                        name,
                        group: "热门".into(),
                        extra: item.clone(),
                    });
                }
                Ok(ProviderResponse::PlaylistTags(list))
            }
            ProviderRequest::Playlists {
                tag,
                sort,
                page,
                limit,
            } => {
                let (page, limit) = page_args(page, limit)?;
                let v=self.weapi("playlist/list",j!({"cat":if tag.is_empty(){"全部"}else{&tag},"order":if sort.is_empty(){"hot"}else{&sort},"limit":limit,"offset":(page as u64-1)*limit as u64,"total":true}),ctx)?;
                Ok(ProviderResponse::Playlists(Page::new(
                    array(&v["playlists"], "playlists")?
                        .iter()
                        .map(playlist_info)
                        .collect(),
                    number(&v["total"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::SearchPlaylists {
                keyword,
                page,
                limit,
            } => {
                let (page, limit) = page_args(page, limit)?;
                let v=self.eapi("/api/cloudsearch/pc",j!({"s":keyword,"type":1000,"limit":limit,"total":page==1,"offset":(page as u64-1)*limit as u64}),ctx)?;
                Ok(ProviderResponse::Playlists(Page::new(
                    array(&v["result"]["playlists"], "result.playlists")?
                        .iter()
                        .map(playlist_info)
                        .collect(),
                    number(&v["result"]["playlistCount"]),
                    page,
                    limit,
                )))
            }
            ProviderRequest::Comments {
                id,
                kind,
                page,
                limit,
                cursor,
                ..
            } => self.comments(&id, kind, page, limit, cursor, ctx),
            ProviderRequest::Picture { track: record } => {
                let id = numeric_id(&text(&record.platform.song_id))?;
                let body = j!({"c":format!("[{{\"id\":{id}}}]"),"ids":format!("[{id}]")});
                let v = self.encrypted(
                    "https://music.163.com/weapi/v3/song/detail",
                    crypto::weapi(&body)?,
                    Some(&format!("https://music.163.com/song?id={id}")),
                    None,
                    ctx,
                )?;
                let url = text(&v["songs"][0]["al"]["picUrl"]);
                if url.is_empty() {
                    return Err(ProviderError::upstream("NetEase picture unavailable"));
                }
                Ok(ProviderResponse::Picture(url))
            }
            ProviderRequest::Lyrics {
                track: record,
                word_by_word,
            } => {
                let id = numeric_id(&text(&record.platform.song_id))?;
                let body =
                    j!({"id":id,"cp":false,"tv":0,"lv":0,"rv":0,"kv":0,"yv":0,"ytv":0,"yrv":0});
                let v = self.encrypted(
                    "https://interface3.music.163.com/eapi/song/lyric/v1",
                    crypto::eapi("/api/song/lyric/v1", &body),
                    None,
                    None,
                    ctx,
                )?;
                let mut lyrics = normalize_lyrics(&v)?;
                if !word_by_word {
                    lyrics.word_by_word = None;
                }
                Ok(ProviderResponse::Lyrics(lyrics))
            }
            other => Err(ProviderError::Unsupported {
                platform: Platform::Wy,
                operation: other.operation().into(),
            }),
        }
    }
}

fn array<'a>(v: &'a Value, name: &str) -> Result<&'a Vec<Value>, ProviderError> {
    v.as_array()
        .ok_or_else(|| ProviderError::parse(format!("missing {name} array")))
}
fn numeric_id(id: &str) -> Result<u64, ProviderError> {
    id.parse::<u64>()
        .map_err(|_| ProviderError::invalid("NetEase ID must be a numeric integer"))
}
fn parse_id(raw: &str) -> Result<u64, ProviderError> {
    if let Ok(id) = numeric_id(raw) {
        return Ok(id);
    }
    for marker in ["?id=", "&id=", "/playlist/"] {
        if let Some((_, tail)) = raw.split_once(marker) {
            let digits: String = tail.chars().take_while(char::is_ascii_digit).collect();
            return numeric_id(&digits);
        }
    }
    Err(ProviderError::invalid("unsupported NetEase playlist link"))
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}
fn optional(v: &Value) -> Option<String> {
    v.as_str().map(str::to_owned)
}
fn track(song: &Value, privilege: &Value) -> Result<TrackRecord, ProviderError> {
    if !song["id"].is_u64() {
        return Err(ProviderError::parse("NetEase song id must remain numeric"));
    }
    let maxbr = number(&privilege["maxbr"]);
    let mut types = Vec::new();
    let mut detail = serde_json::Map::new();
    let mut add = |quality: &str, field: &str| {
        let size = song[field].get("size").map(size).unwrap_or(Value::Null);
        types.push(j!({"type":quality,"size":size}));
        detail.insert(quality.into(), j!({"size":size}));
    };
    if matches!(maxbr, 128000 | 192000 | 320000 | 999000) {
        add("128k", "l");
    }
    if matches!(maxbr, 320000 | 999000) {
        add("320k", "h");
    }
    if maxbr == 999000 {
        add("flac", "sq");
    }
    if privilege["maxBrLevel"] == "hires" {
        add("flac24bit", "hr");
    }
    let singers = song["ar"]
        .as_array()
        .map(|s| {
            s.iter()
                .map(|v| text(&v["name"]))
                .collect::<Vec<_>>()
                .join("、")
        })
        .unwrap_or_default();
    let pc = song.get("pc");
    let name = pc
        .and_then(|p| p["sn"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| text(&song["name"]));
    let album = pc
        .and_then(|p| p["alb"].as_str())
        .map(str::to_owned)
        .unwrap_or_else(|| text(&song["al"]["name"]));
    let singer = pc.and_then(|p| p["ar"].as_str()).unwrap_or(&singers);
    TrackRecord::from_lx_dto(j!({"name":name,"singer":singer,"albumName":album,"albumId":song["al"]["id"],"source":"wy","songmid":song["id"],"interval":playtime(number(&song["dt"])/1000),"img":song["al"]["picUrl"],"lrc":null,"types":types,"_types":detail,"typeUrl":{},"privilege":privilege}),LxMusicDtoVersion::Legacy).map_err(|e|ProviderError::parse(e.to_string()))
}
fn playtime(seconds: u64) -> String {
    if seconds == 0 {
        "--/--".into()
    } else {
        format!("{:02}:{:02}", seconds / 60, seconds % 60)
    }
}
fn playlist_info(v: &Value) -> Playlist {
    Playlist {
        id: text(&v["id"]),
        name: text(&v["name"]),
        image_url: optional(&v["coverImgUrl"]),
        author: optional(&v["creator"]["nickname"]),
        description: optional(&v["description"]),
        play_count: v.get("playCount").map(number),
        track_count: v.get("trackCount").map(number),
        extra: v.clone(),
    }
}
fn size(v: &Value) -> Value {
    if v.is_null() {
        return Value::Null;
    }
    let size = number(v);
    if size == 0 {
        return j!("0 B");
    }
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let power = ((size as f64).log(1024.0).floor() as usize).min(4);
    j!(format!(
        "{:.2} {}",
        size as f64 / 1024f64.powi(power as i32),
        units[power]
    ))
}
fn comment(v: &Value) -> Comment {
    Comment {
        id: text(&v["commentId"]),
        text: emoji(&text(&v["content"])),
        author: text(&v["user"]["nickname"]),
        avatar_url: optional(&v["user"]["avatarUrl"]),
        timestamp_ms: v.get("time").map(number),
        likes: number(&v["likedCount"]),
        replies: Vec::new(),
        extra: v.clone(),
    }
}
fn parse_comments(v: &Value) -> Result<Vec<Comment>, ProviderError> {
    Ok(array(v, "comments")?
        .iter()
        .map(|v| {
            let c = comment(v);
            if let Some(root) = v["beReplied"].as_array().and_then(|r| r.first()) {
                let mut r = comment(root);
                r.id = c.id.clone();
                if let Some(extra) = r.extra.as_object_mut() {
                    extra.insert("rootId".into(), root["beRepliedCommentId"].clone());
                }
                r.timestamp_ms = c.timestamp_ms;
                r.replies.push(c);
                r
            } else {
                c
            }
        })
        .collect())
}
fn emoji(text: &str) -> String {
    let pairs = [
        ("大笑", "😃"),
        ("可爱", "😊"),
        ("憨笑", "☺️"),
        ("色", "😍"),
        ("亲亲", "😙"),
        ("惊恐", "😱"),
        ("流泪", "😭"),
        ("亲", "😚"),
        ("呆", "😳"),
        ("哀伤", "😔"),
        ("呲牙", "😁"),
        ("吐舌", "😝"),
        ("撇嘴", "😒"),
        ("怒", "😡"),
        ("奸笑", "😏"),
        ("汗", "😓"),
        ("痛苦", "😖"),
        ("惶恐", "😰"),
        ("生病", "😨"),
        ("口罩", "😷"),
        ("大哭", "😂"),
        ("晕", "😵"),
        ("发怒", "👿"),
        ("开心", "😄"),
        ("鬼脸", "😜"),
        ("皱眉", "😞"),
        ("流感", "😢"),
        ("爱心", "❤️"),
        ("心碎", "💔"),
        ("钟情", "💘"),
        ("星星", "⭐️"),
        ("生气", "💢"),
        ("便便", "💩"),
        ("强", "👍"),
        ("弱", "👎"),
        ("拜", "🙏"),
        ("牵手", "👫"),
        ("跳舞", "👯‍♀️"),
        ("禁止", "🙅‍♀️"),
        ("这边", "💁‍♀️"),
        ("爱意", "💏"),
        ("示爱", "👩‍❤️‍👨"),
        ("嘴唇", "👄"),
        ("狗", "🐶"),
        ("猫", "🐱"),
        ("猪", "🐷"),
        ("兔子", "🐰"),
        ("小鸡", "🐤"),
        ("公鸡", "🐔"),
        ("幽灵", "👻"),
        ("圣诞", "🎅"),
        ("外星", "👽"),
        ("钻石", "💎"),
        ("礼物", "🎁"),
        ("男孩", "👦"),
        ("女孩", "👧"),
        ("蛋糕", "🎂"),
        ("18", "🔞"),
        ("圈", "⭕"),
        ("叉", "❌"),
    ];
    let mut out = text.to_owned();
    for (name, value) in pairs {
        out = out.replace(&format!("[{name}]"), value);
    }
    out
}
fn charts() -> Vec<Chart> {
    let values = [
        ("19723756", "飙升榜"),
        ("3779629", "新歌榜"),
        ("2884035", "原创榜"),
        ("3778678", "热歌榜"),
        ("991319590", "说唱榜"),
        ("71384707", "古典榜"),
        ("1978921795", "电音榜"),
        ("5453912201", "黑胶VIP爱听榜"),
        ("71385702", "ACG榜"),
        ("745956260", "韩语榜"),
        ("10520166", "国电榜"),
        ("180106", "UK排行榜周榜"),
        ("60198", "美国Billboard榜"),
        ("3812895", "Beatport全球电子舞曲榜"),
        ("21845217", "KTV唛榜"),
        ("60131", "日本Oricon榜"),
        ("2809513713", "欧美热歌榜"),
        ("2809577409", "欧美新歌榜"),
        ("27135204", "法国 NRJ Vos Hits 周榜"),
        ("3001835560", "ACG动画榜"),
        ("3001795926", "ACG游戏榜"),
        ("3001890046", "ACG VOCALOID榜"),
        ("3112516681", "中国新乡村音乐排行榜"),
        ("5059644681", "日语榜"),
        ("5059633707", "摇滚榜"),
        ("5059642708", "国风榜"),
        ("5338990334", "潜力爆款榜"),
        ("5059661515", "民谣榜"),
        ("6688069460", "听歌识曲榜"),
        ("6723173524", "网络热歌榜"),
        ("6732051320", "俄语榜"),
        ("6732014811", "越南语榜"),
        ("6886768100", "中文DJ榜"),
        ("6939992364", "俄罗斯top hit流行音乐榜"),
        ("7095271308", "泰语榜"),
        ("7356827205", "BEAT排行榜"),
        (
            "7325478166",
            "编辑推荐榜VOL.44 天才女子摇滚乐队boygenius剖白卑微心迹",
        ),
        ("7603212484", "LOOK直播歌曲榜"),
        ("7775163417", "赏音榜"),
        ("7785123708", "黑胶VIP新歌榜"),
        ("7785066739", "黑胶VIP热歌榜"),
        ("7785091694", "黑胶VIP爱搜榜"),
    ];
    values
        .into_iter()
        .map(|(id, name)| Chart {
            id: format!("wy__{id}"),
            name: name.into(),
            image_url: None,
            description: None,
            extra: j!({"bangid":id}),
        })
        .collect()
}

fn time(ms: u64) -> String {
    format!("[{:02}:{:02}.{:03}]", ms / 60000, ms / 1000 % 60, ms % 1000)
}
fn headers(raw: &str) -> Vec<String> {
    raw.replace('\r', "")
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("{\"") {
                let v = serde_json::from_str::<Value>(line).ok()?;
                let ms = v["t"].as_u64()?;
                let words = v["c"]
                    .as_array()?
                    .iter()
                    .map(|t| text(&t["tx"]))
                    .collect::<String>();
                Some(format!("{}{words}", time(ms)))
            } else {
                Some(fix_label(line))
            }
        })
        .collect()
}
fn fix_label(line: &str) -> String {
    // The legacy [mm:ss:xx] tag occurs in LRC translations from this endpoint.
    if let Some((tag, words)) = line.strip_prefix('[').and_then(|s| s.split_once(']'))
        && tag.matches(':').count() == 2
        && !tag.contains('.')
        && let Some((a, b)) = tag.rsplit_once(':')
    {
        return format!("[{a}.{b}]{words}");
    }
    line.into()
}
fn timestamp(line: &str) -> Option<(u64, &str, &str)> {
    let end = line.find(']')?;
    let label = &line[..=end];
    let tag = label.strip_prefix('[')?.strip_suffix(']')?;
    let (m, sec) = tag.split_once(':')?;
    let (s, f) = sec.split_once('.').unwrap_or((sec, "0"));
    let millis = f.chars().take(3).collect::<String>();
    let fraction = millis.parse::<u64>().ok()? * 10u64.pow(3 - millis.len() as u32);
    Some((
        m.parse::<u64>().ok()? * 60000 + s.parse::<u64>().ok()? * 1000 + fraction,
        label,
        &line[end + 1..],
    ))
}
fn align(base: &str, translated: &str) -> String {
    let mut remaining: Vec<_> = base.lines().filter_map(timestamp).collect();
    let mut out = Vec::new();
    for line in headers(translated) {
        if let Some((t, _, words)) = timestamp(&line) {
            if words.trim().is_empty() {
                continue;
            }
            if let Some(index) = remaining.iter().position(|(s, _, _)| s.abs_diff(t) < 100) {
                let (_, tag, _) = remaining.remove(index);
                out.push(format!("{tag}{words}"));
            }
        }
    }
    out.join("\n")
}
pub fn normalize_lyrics(value: &Value) -> Result<Lyrics, ProviderError> {
    let mut result = Lyrics::default();
    let raw = text(&value["yrc"]["lyric"]);
    if !raw.trim().is_empty() {
        let mut lrc = Vec::new();
        let mut lx = Vec::new();
        for line in headers(&raw) {
            if let Some((_, _, _)) = timestamp(&line) {
                lrc.push(line.clone());
                continue;
            }
            if line.starts_with("[offset") {
                lrc.push(line.clone());
                lx.push(line);
                continue;
            }
            let Some((tag, body)) = line.strip_prefix('[').and_then(|s| s.split_once(']')) else {
                continue;
            };
            let Some((start, _)) = tag.split_once(',') else {
                continue;
            };
            let Ok(start) = start.parse::<u64>() else {
                continue;
            };
            let plain = YRC_WORD.replace_all(body, "").into_owned();
            let mut timed = String::new();
            let tokens: Vec<_> = YRC_WORD.captures_iter(body).collect();
            for (index, capture) in tokens.iter().enumerate() {
                let at = capture[1]
                    .parse::<u64>()
                    .map_err(|e| ProviderError::parse(e.to_string()))?;
                let duration = capture[2]
                    .parse::<u64>()
                    .map_err(|e| ProviderError::parse(e.to_string()))?;
                let begin = capture.get(0).expect("YRC match").end();
                let end = tokens
                    .get(index + 1)
                    .map(|next| next.get(0).expect("YRC match").start())
                    .unwrap_or(body.len());
                let word = &body[begin..end];
                timed.push_str(&format!(
                    "<{},{}>{word}",
                    at.saturating_sub(start),
                    duration
                ));
            }
            lrc.push(format!("{}{plain}", time(start)));
            if !timed.is_empty() {
                lx.push(format!("{}{timed}", time(start)));
            }
        }
        result.lyric = lrc.join("\n");
        result.word_by_word = Some(lx.join("\n"));
        let tl = text(&value["ytlrc"]["lyric"]);
        let rl = text(&value["yromalrc"]["lyric"]);
        result.translated = Some(align(&result.lyric, &tl));
        result.romanized = Some(align(&result.lyric, &rl));
    } else {
        result.lyric = headers(&text(&value["lrc"]["lyric"])).join("\n");
        result.translated = Some(headers(&text(&value["tlyric"]["lyric"])).join("\n"));
        result.romanized = Some(headers(&text(&value["romalrc"]["lyric"])).join("\n"));
    }
    if result.lyric.trim().is_empty() {
        return Err(ProviderError::upstream("NetEase lyrics unavailable"));
    }
    result.extra = j!({"source":"wy"});
    Ok(result)
}
