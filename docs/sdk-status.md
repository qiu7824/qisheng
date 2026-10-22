# LX Native 五平台 SDK 状态

本文对应 **0.3.0-alpha.11**、SQLite schema **8**，兼容基线为 LX Music Desktop **v2.12.6**，固定提交 `ad95d5091c9ed689fa72b5e5c849df65f5a679ce`。内置 SDK 由 Rust adapter 实现，公开入口是 `platforms::ProviderRegistry`；活动能力按固定导出及实际调用链实现，未导出的能力返回 `ProviderError::Unsupported`。

## 活动能力

表内“支持”表示实现及协议 fixture 覆盖，不表示每个接口分支都通过真实服务验收。平台标识为 `kw`（酷我）、`tx`（QQ 音乐）、`kg`（酷狗）、`wy`（网易云音乐）、`mg`（咪咕）。

| ProviderRequest operation | kw | tx | kg | wy | mg |
| --- | --- | --- | --- | --- | --- |
| `search` | 支持 | 支持 | 支持 | 支持 | 支持 |
| `hot_search` | 支持 | 支持 | 支持 | 支持 | 支持 |
| `suggest` | 支持 | 未导出 | 未导出 | 未导出 | 未导出 |
| `detail` | 支持 | 支持 | 支持 | 支持 | 支持 |
| `charts` / `chart_tracks` | 支持 | 支持 | 支持 | 支持 | 支持 |
| `playlist_tags` / `playlists` | 支持 | 支持 | 支持 | 支持 | 支持 |
| `search_playlists` / `playlist_detail` | 支持 | 支持 | 支持 | 支持 | 支持 |
| `album` | 支持 | 无活动调用 | 无活动调用 | 无活动调用 | 无活动调用 |
| `comments` normal/hot | 支持 | 支持 | 支持 | 支持 | 支持 |
| `comments` replies | 未导出 | 无独立活动调用 | 支持 | 无独立活动调用 | 支持 |
| `lyrics` / `picture` | 支持 | 支持 | 支持 | 支持 | 支持 |

歌手模块没有注册活动入口。回复可以包含在 normal/hot 评论中，不能据此推导独立 replies 操作可用。榜单目录保留基线固定列表；`charts` 成功代表目录返回，`chart_tracks` 才访问相应榜单歌曲接口。

| 平台 | 协议和数据约束 |
| --- | --- |
| kw | 保留 rid、质量与 digest；榜单加密、单引号搜索响应、歌单 digest/菠点分支、普通/热门评论、封面与二进制歌词。官方 www 详情使用匿名公开会话及 Secret；握手与刷新次数有限 |
| tx | 保留数字 song ID、songmid、albumMid、strMediaMid；原生请求签名、固定 headers、榜单 period、歌单 fallback、普通/热门评论、原生 QRC 解码 |
| kg | 保留 album_audio_id、hash 与质量 hash；网关签名、special/分享码/chain/GCID 歌单、fallback、固定服务分页、评论及 KRC 四轨；详情批次中空对象表示不可用占位，非空坏 schema 仍报 Parse |
| wy | JSON 数字歌曲 ID、privilege.maxbr/maxBrLevel、AES eapi/weapi/linuxapi 与固定公钥 RSA；评论前进/后退/重放及显式 cursor；YRC、翻译和罗马音对齐 |
| mg | songId 与 copyrightId 分离，保留 lrcUrl/mrcUrl/trcUrl；固定设备签名、最多 100 项详情批次、歌单结构、按曲目/页大小隔离的评论 cursor、MRC/LRC/翻译歌词 |

`TrackRecord` 使用现代/旧 LX DTO 转换，保留平台扩展元数据和质量信息。内置 SDK 不包含 `musicUrl`；该动作由经批准的自定义源提供。后端已接列表保存、按列表副本播放、质量协商、音频 URL 审批及内置歌词路由；元数据质量本身仍不证明对应音频已获取或可播放，真实候选匹配/换源与URL信息缓存已接入，持久图片/音频复用及有限HTTP恢复已接通；本地匹配、其他恢复分支和完整真实服务联测尚未完成。

## 后端与 CLI

`BackendCommand::ProviderRequest` 接受平台及 typed request，在独立工作线程执行。控制线程不等待 HTTP；Accepted receipt 的 `operation_id` 可用于 `provider_result`、事件查询和取消。`ProviderFinished` 报告终态，结果查询返回 `ProviderOperationResult`，其中响应为 Tracks、Keywords、Charts、PlaylistTags、Playlists、Collection、Comments、Lyrics 或 Picture。

```json
{"request_id":"wy-search-1","command":"provider_request","platform":"wy","request":{"operation":"search","keyword":"周杰伦","page":1,"limit":30}}
{"query":"provider_result","operation_id":"wy-search-1"}
{"request_id":"wy-cancel-1","command":"cancel_operation","operation_id":"wy-search-1"}
```

终态为 completed 或 failed；取消以 failed/Cancelled 表示。错误区分 Unsupported、InvalidRequest、Cancelled、DeadlineExceeded、Network、Http、ResponseTooLarge、Parse 和 Upstream，接口失败不会转换成虚假的空列表成功。分页请求要求 page≥1、limit 为 1–1000；评论 cursor 的具体规则由 adapter 保留。

```powershell
cargo run --locked --no-default-features --bin lx-native-cli -- search-online wy "周杰伦"
cargo run --locked --no-default-features --bin lx-native-cli -- --json-lines
```

单次 `search-online` 等待终态；JSON Lines 立即返回 receipt，后续输入可继续操作。普通 SDK 查询不会自动保存或播放。在同一后端会话内，completed 的 Tracks/Collection 结果可通过 `save_provider_tracks` 保存到目标列表；`music_ids` 可匹配返回的 native id 或 lx_id，空数组保存该返回页的全部曲目。结果若已逐出或不含曲目则返回错误，另一个单次 CLI 进程不能复用旧 operation_id。

```json
{"request_id":"save-wy-1","command":"save_provider_tracks","operation_id":"wy-search-1","list_id":"default","location":"bottom"}
{"query":"list_tracks","list_id":"default","offset":0,"limit":100}
{"request_id":"select-default-1","command":"select_list","list_id":"default"}
{"query":"queue","offset":0,"limit":100}
```

`play_list_track` 使用 `list_id/music_id` 读取列表独立文档；可选 `quality` 严格匹配曲目和已批准源的声明，不支持时返回 Unavailable。SourceWorker 解析得到陌生音频 URL 后等待 `approve_playback_url`，审批前不进行音频下载。临时队列保留来源列表引用；普通条目移除同步删除对应持久成员，temporary 仅修改 FIFO，虚拟 catalog 成员只读。播放检查点保存队列和控制参数，重启保持暂停候选，不自动运行源或下载。

LyricService 已按当前列表副本和本地/编辑/raw cache 优先级调用内置平台 resolver，保持取消与音频 generation 校验。实际worker、流式HTTP、Memory PCM与精确批准的预加载链路已有集成测试；匹配与持久URL信息缓存见[媒体解析接口](media-resolution.md)；图片缓存、跨播放编码复用及有限HTTP恢复见[图片接口](artwork-cache.md)与[编码复用](encoded-reuse.md)；本地匹配、其他恢复分支与完整服务联测仍待完成，M05仍未整体完成。

| 内置 SDK 调度/传输 | 行为 |
| --- | --- |
| 操作时限 | 默认 20 秒；CLI `--provider-timeout-ms` 为 1–120000 毫秒 |
| 并行与缓存 | 最多 4 个 provider 工作任务；会话结果缓存最多 32 项/64 MiB，终态结果可能逐出 |
| 响应限额 | 单次 HTTP 默认 8 MiB；TransportConfig 可显式配置，最高 64 MiB |
| 代理/TLS | 默认不读取系统或环境代理；`--provider-proxy` 显式配置；保持证书校验 |
| 重定向/重试 | 最多 5 次重定向；GET/HEAD 对网络错误及限定 HTTP 状态最多额外重试 2 次；POST 不由传输层重试 |
| 取消 | ProviderContext 贯穿请求、重试与返回校验；取消或超时的迟到结果不按成功发布 |

内置 SDK 的传输配置与自定义脚本 HTTP 策略独立。脚本 HTTP 仍执行源批准、地址过滤、固定 DNS 连接及自身较小的资源限额，内置 SDK 代理设置不会扩大脚本能力。

## 歌词 codec 与源兼容

`lyrics::codec` 实现原生 KRC 容器解码、QRC 密文/XML 转换、MRC 有符号 64 位运算及 UTF-16LE 解码。KW 二进制动态歌词和 WY YRC 在对应 adapter 转成普通与 LX 字歌词；翻译/罗马音、时间标签、offset 和正文括号都有转换 fixture。同名 KRC 旁车已进入本地歌词读取链路。

源运行时的方法面与其边界如下；完整 Node/浏览器运行环境不属于该接口。

| 能力 | 可用行为 | 有界差异 |
| --- | --- | --- |
| Buffer | 编码、静态构造、共享/复制视图、搜索/比较、整数、Float/Double、BigInt64、swap、JSON、inspect；Node 24.19 方法面 fixture | 单分配最多 2 MiB；不采用 Node slab pool，allocUnsafe 清零；受 worker 内存预算约束 |
| RSA helper | `lx.utils.crypto.rsaEncrypt`：128 字节左补零、公钥 RSA_NO_PADDING；SPKI/PKCS#1 公钥或未加密私钥的公钥部分 | 仅 1024 位；PEM≤16 KiB；不提供私钥解密、OAEP 或签名 |
| timers | setTimeout/setInterval/clearTimeout/clearInterval；Promise 与 timer 回调顺序经过 fixture | delay 1–60000 ms；pending 64、单 operation 创建 256/回调 512；仅活动 operation 内运行，终态和取消清理 |
| host event | updateAlert 与 openDevTools 请求经 typed IPC/BackendEvent 投影 | updateAlert 每会话一次；host event 单 operation 16、缓存 64；不自动打开浏览器、更新软件或创建 DevTools UI |

源的 manifest 动作仍限定为各平台 `musicUrl`，以及 local `pic/lyric`。工具方法面不改变脚本 SHA-256/策略版本批准、切源撤销、HTTP 地址过滤和 IPC 限额。普通同步 JavaScript 不具备按 timer 时限抢占的保证；进程期限、引擎循环/微任务与资源预算共同限制执行。

## 验证范围

### 协议、算法与故障路径

每个 adapter 的活动操作由 `tests/platform_kw.rs`、`platform_tx.rs`、`platform_kg.rs`、`platform_wy.rs`、`platform_mg.rs` 覆盖，包括注入 HTTP transport 的请求 URL、headers、body、签名、响应字段、ID/质量、分页、fallback 及取消/错误路径。WY RSA/双层 AES、MG/TX/KG 签名与 KW 官方 Secret 使用独立协议 golden，不能用被测输出替换预期值。

`tests/lyric_codecs.rs` 及平台歌词测试使用实际供应商格式的容器/密文 fixture，验证 KRC 四轨、QRC XML 原始换行、MRC 有符号回绕和 KW/WY 字时间转换。`source_complete_utils.rs`、`source_buffer_surface.rs`、`source_timers.rs` 覆盖实际 worker/IPC 与工具兼容；`backend_platforms.rs` 和 CLI 验证覆盖异步运行、查询、结果预算、取消和关机。这些离线结果不证明全部官方服务始终可用。

### 官方服务样例调用

2026-10-06 的 CLI/官方请求样例验证范围如下；“通过”限定于所选接口、曲目及页：

| 路径 | kw | tx | kg | wy | mg |
| --- | --- | --- | --- | --- | --- |
| 搜索 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 热词 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 固定榜单目录调用 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 歌单标签 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 歌曲详情样例 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 封面 URL 样例 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 评论样例路径 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 榜单歌曲页 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 歌单列表样例 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 歌单详情样例 | 通过 | 通过 | 通过 | 通过 | 通过 |
| 非空歌单搜索 | 通过 | 合法空结果 | 通过 | 通过 | 通过 |
| 歌词样例 | 榜单样例通过 | 通过 | 通过 | 通过 | 通过 |
| 建议/专辑活动入口 | 通过 | 不适用 | 不适用 | 不适用 | 不适用 |

KW 详情的匿名官方会话/Secret、WY 标签的公钥请求、KW 建议和专辑有实证。KW 榜单曲目的普通/逐字歌词成功，另一个曲目返回 `TP=none`，表示该曲目歌词不可用；该结果不作为 codec 失败或所有曲目可用的证据。

KG special 歌单样例的 Rust 调用返回 64 条可用记录并通过指定三页验证；只有完全空对象的不可用占位会跳过，全部不可用返回 typed Upstream，非空坏 schema 返回 Parse。TX `search_playlists` 样例返回合法零项，证明响应路径正常，不证明已取得非空歌单搜索结果。WY Album 在基线无活动调用，Unsupported 为预期合同。

封面联测返回 URL，不覆盖全部图片下载和显示。固定榜单目录与真实 `chart_tracks` 页分别记录；所选榜单页成功不代表全目录全部页可用。评论结果也不覆盖所有 cursor 跳转、分页及回复接口。歌词只记录返回状态、格式和转换结果，不展示正文。

全部歌单分类/分享/fallback、完整榜单分页、评论回复、全部在线歌词格式和所有质量仍未逐项真实服务验收。未联测项保留实现与 fixture 状态，不以空响应、空列表或质量标记替代非空功能证据。五平台官方元数据/歌词联测、自建 HTTP/SourceWorker/Memory 播放链路、Device 发声、大曲库响应性能及后续下载/同步/OpenAPI/DSP 分别验收。
