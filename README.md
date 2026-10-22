# 栖声 · 桌面客户端

栖声是 Rust 音乐播放器工程，提供独立后端、JSON CLI 和 ZSUI 桌面界面。后端可以使用 `--no-default-features` 独立构建和运行；桌面界面通过适配层使用同一命令、状态和持久化服务。GPUI Kit 界面保留在可选的 `ui` feature。Windows x64、macOS arm64/x64、Linux x64 的原生构建、测试与打包规则见[三平台交付设计](docs/cross-platform-delivery.md)。

当前源码版本为 **0.3.0-alpha.20**，SQLite schema 为 **17**。可用服务包括本地曲库目录授权/后台扫描、受控 LX 自定义源、四轨歌词、五平台 SDK、事务列表/收藏/不喜欢规则/搜索历史、持久五模式队列，设备选择/热切换/静音、十段均衡器、倍速保调与独立移调，以及按块传输、独立解码和预加载的播放链路。在线 SDK 结果可以显式保存到列表，从该列表独立副本经已批准源解析 URL、音频 URL 审批、Memory/Device 播放路由及内置在线歌词服务。

M05 尚未全部完成：跨平台匹配、真实候选换源和持久URL信息缓存已接入；持久图片缓存、跨播放/冷启动编码音频复用及有限HTTP恢复已接入；本地跨平台匹配、其他错误恢复分支和完整服务联测仍待完成。目录监听、重定位、持久下载、原生逻辑备份、v4 同步、OpenAPI 与系统控制已有后端执行链路；独立批准的 MP3/FLAC 标签编辑、实际资源退休及七个命令和四个查询见[标签 API](docs/backend-tag-edit-api.md)。标签完整冷恢复、下载覆盖与成品播放、完整物理恢复交接、音效偏好及实机与互通验收仍待完成。能力边界见 [后端状态](docs/backend-status.md)与[五平台 SDK 状态](docs/sdk-status.md)；完整工作分解见 [后端移植规划](docs/backend-port-plan.md)。

[桌面 EXE](bin/lx-gpui-player.exe)、`delivery/LX-Native-0.2.0-source.zip` 和 `delivery/LX-Native-0.2.0-Windows-x64.zip` 是保存的 0.2.0 发行产物。以下构建命令和后端说明对应当前源码；旧包的验证记录见 [验收记录](docs/validation.md)。

历史后端产物 [CLI 0.3.0-alpha.14](bin/lx-native-cli-0.3.0-alpha.14.exe)，对应 schema 16；[alpha.14 检查点](docs/backend-checkpoint-alpha14.md)记录严格 Clippy、338 项库测试通过及一项缓存供给性能失败、60 项集成测试通过。标签编辑与实际退休具有完整正常执行链路，剩余冷恢复窗口保持 pending。历史记录为 [alpha.13](docs/backend-checkpoint-alpha13.md)、[alpha.12](docs/backend-checkpoint-alpha12.md)与[alpha.11](docs/backend-checkpoint-alpha11.md)；其范围不作为 alpha.14 的完整发行验收。[匹配与URL缓存接口](docs/media-resolution.md)、[编码复用](docs/encoded-reuse.md)及[图片接口](docs/artwork-cache.md)记录独立授权与限额。其他历史记录保留为[alpha.10](docs/backend-checkpoint-alpha10.md)、[alpha.9](docs/backend-checkpoint-alpha9.md)、[alpha.8](docs/backend-checkpoint-alpha8.md)、[alpha.7](docs/backend-checkpoint-alpha7.md)、[alpha.6](docs/backend-checkpoint-alpha6.md)、[alpha.5](docs/backend-checkpoint-alpha5.md)、[alpha.1](docs/backend-checkpoint.md)、[alpha.2](docs/backend-checkpoint-alpha2.md)与[alpha.3](docs/backend-checkpoint-alpha3.md)。

[后端 CLI 0.3.0-alpha.1](bin/lx-native-cli-0.3.0-alpha.1.exe) 保留本地数据、播放和歌词服务，默认使用 Memory 音频模式，验证范围见[schema 3 检查点](docs/backend-checkpoint.md)。

## 构建

Windows x64 使用 Rust stable、MSVC 构建工具及 Windows SDK。`Cargo.lock` 锁定依赖；主要依赖包括 `gpui-kit 0.7.0`、`symphonia 0.5.5`、`cpal 0.17.3`、`Boa 0.22.0`、`rusqlite 0.40.2` 和 `lofty 0.25.4`。SQLite 使用 bundled 静态库，不需要另行安装 SQLite 服务。

构建缓存、测试临时文件与操作日志使用独立实际目录：

```powershell
$env:CARGO_HOME = 'E:\codex操作目录\lx-gpui-build-20261004\cargo'
$env:CARGO_TARGET_DIR = 'E:\codex操作目录\cache\lx-gpui-player\rust-1.99.0-windows-x86_64-msvc\target'
$env:TEMP = 'E:\codex操作目录\tasks\lx-gpui-player\temp'
$env:TMP = $env:TEMP
New-Item -ItemType Directory -Force -Path $env:CARGO_TARGET_DIR, $env:TEMP | Out-Null

cargo fmt --all -- --check
cargo check --locked --no-default-features --all-targets
cargo test --locked --no-default-features --all-targets -- --test-threads=1
cargo build --locked --no-default-features --bin lx-native-cli
```

桌面界面使用默认 `desktop` feature，ZSUI 源码及其许可证位于 `vendor/zsui`：

```powershell
cargo check --locked --all-targets
cargo test --locked --all-targets
cargo build --locked --bin lx-native
cargo run --locked --bin lx-native
```

桌面入口支持 `--portable`、`--data-dir PATH` 和文件或目录参数；`--audio memory` 使用内存音频输出。启用 `smoke` feature 后，可使用 `--smoke PATH.png` 执行原生窗口截图测试。

GPUI Kit 界面可使用 `cargo build --locked --no-default-features --features ui --bin lx-gpui-player` 单独构建。

硬件音频与原生窗口验证单独执行：

```powershell
cargo test --locked audio::tests::real_audio_device_playback_diagnostic -- --ignored --nocapture
cargo test --locked --no-default-features --features ui --test ui capture_real_gpui_library_png -- --ignored --nocapture
```

Memory 模式使用显式 Symphonia 解码和与 CPAL 相同的渲染内核，验证播放、暂停、定位、音量、切歌及预加载交接。该模式不打开系统输出设备；扬声器发声、设备驱动和插拔需要实机验证。

## CLI

CLI 默认使用 Memory 音频；`--audio device` 选择系统输出。Windows 普通安装使用 `%LOCALAPPDATA%\LXNative`；`--portable` 使用可执行文件旁的 `userdata`；`--data-dir PATH` 指定独立 profile，优先于 portable。桌面入口采用同样的目录规则。

```powershell
cargo run --locked --no-default-features --bin lx-native-cli -- status
cargo run --locked --no-default-features --bin lx-native-cli -- search "First Light"
cargo run --locked --no-default-features --bin lx-native-cli -- search-online wy "周杰伦"
cargo run --locked --no-default-features --bin lx-native-cli -- lists
cargo run --locked --no-default-features --bin lx-native-cli -- list default
cargo run --locked --no-default-features --bin lx-native-cli -- queue
cargo run --locked --no-default-features --bin lx-native-cli -- import-local 'E:\音乐\本地曲目.flac'
cargo run --locked --no-default-features --bin lx-native-cli -- --json-lines
```

JSON Lines 每行输入一个 JSON 对象，每行返回 `{"ok":true,"result":...}` 或 `{"ok":false,"error":...}`。异步命令立即返回 Accepted receipt，可在同一连接继续查询状态、读取事件或取消；单次命令模式等待异步终态。输入 EOF 触发后端关机。

```json
{"query":"status"}
{"query":"tracks","text":"Violet","offset":0,"limit":10}
{"request_id":"volume-1","command":"volume","volume":0.5}
{"request_id":"play-1","command":"play","id":"tone-1"}
{"query":"events","cursor":0}
{"request_id":"pause-1","command":"pause"}
{"request_id":"seek-1","command":"seek","seconds":5.0}
```

`request_id` 在同一后端实例的有限缓存窗口内支持幂等重放；相同 ID 的不同输入返回 Conflict。`expected_revision` 可保护有版本的修改。异步操作使用 receipt 的 `operation_id`，终态由 `operation_finished` 事件报告。事件保留最近 256 条，游标限当前进程。

开始事件订阅或游标过期时，读取原子快照与游标配对，然后用返回的 `cursor` 继续查询：

```json
{"query":"snapshot_cursor"}
{"query":"event_cursor"}
```

`snapshot_cursor` 返回 `result.snapshot` 和 `result.cursor`；`event_cursor` 只返回当前游标。需要重建状态时使用前者，避免先读取状态、再读取游标期间遗漏变更。

内置 SDK 的平台标识为 `kw/tx/kg/wy/mg`。`provider_request` 在独立工作线程执行，返回 Accepted receipt；通过 `provider_result` 查询同一 `operation_id` 的运行状态、类型化响应或错误。单次 `search-online` 等待终态，JSON Lines 可在请求期间继续查询、取消及控制本地播放。

```json
{"request_id":"search-wy-1","command":"provider_request","platform":"wy","request":{"operation":"search","keyword":"周杰伦","page":1,"limit":30}}
{"query":"provider_result","operation_id":"search-wy-1"}
{"request_id":"cancel-wy-1","command":"cancel_operation","operation_id":"search-wy-1"}
```

`--provider-proxy URL` 显式配置内置 SDK 代理；默认不读取环境或系统代理。`--provider-timeout-ms` 允许 1–120000 毫秒，默认 20000 毫秒。搜索结果返回 `TrackRecord`，不会自动保存或播放；同一 JSON Lines 会话内，确认结果 completed 后可执行 `save_provider_tracks`，空 `music_ids` 表示保存该返回页的全部曲目。活动接口、ID、分页和已联测范围见 [SDK 状态](docs/sdk-status.md)。

```json
{"request_id":"save-search-1","command":"save_provider_tracks","operation_id":"search-wy-1","list_id":"default","location":"bottom"}
{"query":"list_tracks","list_id":"default","offset":0,"limit":100}
{"request_id":"select-default-1","command":"select_list","list_id":"default"}
{"request_id":"mode-1","command":"set_play_mode","mode":"random"}
{"query":"queue","offset":0,"limit":100}
{"query":"playback_checkpoint"}
```

`play-list LIST_ID MUSIC_ID` 或 `play_list_track` 使用列表查询返回的 `id/lx_id`。该命令的可选 `quality` 是严格要求，不支持时返回 Unavailable，不静默选择更低质量。陌生音频 URL 等待 `approve_playback_url`；审批前不发音频下载请求。本地已授权文件桥采用独立本地路径权限。

普通 `remove_queue_entry` 会移除对应持久列表的成员；temporary 条目仅修改临时 FIFO，不删除持久列表。`catalog` 是只读虚拟集合，需先保存到持久列表再修改成员。持久 `temp` 列表与临时 FIFO 分别建模。

已批准源的下一曲预加载在已知时长超过10秒、剩余不足10秒且seek后已间隔3秒时触发。`preload` 查询返回候选、操作ID、实际质量、状态与待批URL。`approve_preload_url` 必须使用该操作ID和逐字匹配的URL；`cancel_preload` 撤销同一操作。准备受理返回Accepted，Ready或失败后产生OperationFinished；自然结束时使用原decoder/transport/PCM资源提升，资源失效则重新解析并等待新的精确批准。投机解析和local歌词沿用同一JS会话，前台解析优先，普通取消保留闭包状态。

```json
{"query":"preload"}
{"query":"audio_cache_stats"}
{"request_id":"clear-audio-1","command":"clear_audio_cache"}
```

预加载默认累计HTTP DATA预算2 MiB、PCM队列80 ms/预填40 ms；TCP/TLS预收独立于DATA计量。缓存清理采用受管文件登记与owner锁，活动块延期清理，未知文件和链接保留并报告cannotClear。缓存统计包含实际物理元数据，encoded_bytes归零后disk_bytes可以仍非零；跨播放及重启音频复用尚未实现。

歌词接口也可通过 JSON Lines 调用：

```json
{"request_id":"edit-1","command":"save_lyrics","id":"tone-1","bundle":{"lyric":"[00:00.00]原创歌词","tlyric":null,"rlyric":"","lxlyric":"[00:00.00]<0,1000>原创歌词"}}
{"query":"lyrics","id":"tone-1","origin":"edited"}
{"query":"lyrics_export","id":"tone-1","format":"bundle_json"}
{"request_id":"restore-1","command":"restore_lyrics","id":"tone-1"}
```

导出返回文本、扩展名和 MIME，调用方负责保存文件。`load_lyrics` 适用于当前播放曲目；切歌和 seek 使用音频 generation 与 seek 序列隔离过期结果。

## 本地数据与音频

`native-library.sqlite3` 保存曲目、稳定文件映射、raw/edited 四轨歌词、default/love/temp/user 列表、来源及位置、不喜欢文本规则、最多 15 项搜索历史、播放检查点、revision、outbox 与迁移标记；`player-state.json` 继续保存源脚本及精确批准、选择和音量。profile 使用独占写锁。首次启动迁移已有 JSON 文件映射，保留原 JSON 和批准记录；schema 升级前保存数据库备份，迁移失败回滚，未知 schema 和损坏数据不会静默重建覆盖。

列表、队列及控制参数可通过 typed command/query 和 CLI 使用。跨列表移动、内容修改与 outbox 共用事务，`expected_revision` 拒绝过期写入；播放读取对应列表的独立副本。播放检查点保存队列、位置、质量、错误跳过/切源开关和歌词 offset，重启为暂停候选，不自动创建 SourceWorker、下载或发声。检查点专用上限 32 MiB，其他仓库文档仍为 8 MiB；50000 条持久化重启已验证，控制延迟 p95 与完整大曲库性能仍需独立测量。schema 7 不能交给只支持旧 schema 的二进制写入。

本地曲目 ID 由规范路径身份生成，重复导入和重启保持一致；loopback URL 的临时端口不作为身份。当前处理显式选择的 WAV、MP3、FLAC、OGG/OGA、M4A/MP4/AAC 文件；标签可读时采用真实标题、艺术家和专辑，标签失败时保留音频并报告警告。显式目录发现、actor 后台入库、roots 持久仓、监听和重定位见[本地曲库接口](docs/local-library.md)。标签写入另需独立批准，接口与故障边界见[标签编辑 API](docs/backend-tag-edit-api.md)。

本地 HTTP 桥按文件流读取，支持 GET/HEAD、单段 Range、ETag/If-Range、206/416 与有限传输缓冲。播放器按 256 KiB 块传输，显式解码线程向有限 PCM 队列供给内容；生产路径已移除整首 64 MiB 与总加载 20 秒限制。等待数据的 idle timeout 仍为 20 秒，主动背压与投机预算等待不计入该期限。68 MiB 原创 WAV 和第二块持续传输 21 秒的真实 Memory 播放已通过专项验证。

## 自定义源与授权

兼容基线为官方 LX Music Desktop v2.12.6，固定提交 `ad95d5091c9ed689fa72b5e5c849df65f5a679ce`。详细协议见 [上游契约](docs/upstream-contract.md)。

源导入只保存元数据、字节摘要和语法校验结果，不执行脚本。批准绑定精确脚本 SHA-256 和策略版本；内容改动或旧策略失配撤销批准。显式批准并启用后才创建独立运行会话；停用、删除、切换、取消和关机终止旧会话及在途 HTTP。已批准的选择可在重启后重新初始化。

工程的 `fixtures/legal-local-source.js` 与 `fixtures/legal-http-utils-source.js` 是原创本地演示源。静态安装示例采用单次命令，等待保存完成后退出：

```powershell
$sourcePath = (Resolve-Path '.\fixtures\legal-local-source.js').Path
$request = @{ request_id = 'source-import-1'; command = 'import_source'; path = $sourcePath } | ConvertTo-Json -Compress
cargo run --locked --no-default-features --bin lx-native-cli -- --command $request
```

安装记录保持未批准；`approve_source` 的 `id` 使用安装结果中的脚本 SHA-256。执行前应检查完整脚本与联网权限，授权不会因源名称、文件路径或备份导入而继承。

运行时支持 `musicUrl`、local `pic/lyric`、受控 HTTP、Buffer 方法面、MD5、AES-CBC/ECB、LX RSA_NO_PADDING helper、zlib 和有界 timers。Buffer 覆盖整数、Float/Double、64 位 BigInt、编码、视图、搜索、复制及检查方法；RSA 仅执行 1024 位公钥运算。`updateAlert/openDevTools` 请求经 typed host event 返回，宿主不自动更新软件或打开 DevTools。local `lyric` 返回类型化四轨对象，经 JS→worker IPC→Rust DTO 校验。

`setTimeout/setInterval` 仅在当前初始化或 resolver operation 内运行；完成、取消和切源清除遗留回调。Buffer 分配有 2 MiB 上限，`allocUnsafe` 仍返回清零内存。运行时不提供 DOM、Node 文件系统或永久后台事件循环。五平台搜索、榜单、歌单、歌词与封面通过独立 Rust provider；SDK 不提供内置音频播放 URL，实际播放路由使用已批准的源和单独音频 URL 审批。

脚本最大 1 MiB，IPC 最大 2 MiB。批准后的 HTTP 允许公网和演示 loopback，拒绝 LAN/link-local/元数据地址；保持 TLS 校验，不跟随重定向、不读取系统代理或自动附带 Cookie/凭证。具体时限、歌词、元数据和封面限额见 [执行与资源限制](docs/backend-status.md#执行与资源限制)。

## 验证与许可

验证分别记录静态检查、自动化测试、真实 worker/HTTP 互通、Memory PCM 和硬件音频。五平台协议 fixture 及 2026-10-06 官方样例调用覆盖搜索、热词、标签、详情、封面、评论、榜单歌曲、歌单列表/详情及歌词；非空歌单搜索覆盖 KW/KG/WY/MG，TX 样例为合法空结果。KW 建议、专辑与普通/逐字榜单歌词有样例验证；一个 KW 曲目返回 TP=none，按不可用结果处理。全部分页、分享/fallback 分支、评论回复及所有质量仍未逐项联测。

后端专项测试覆盖实际 SDK HTTP→default/love 独立副本→SourceWorker→审批前零音频请求→审批后 Memory PCM/内置歌词、严格质量、解析失败/错误跳过期间保持暂停、当前曲目删除和重启暂停恢复。列表工具的 ICU/UTF-16 原索引操作、1505 条 SQL 分页、50000 条检查点持久化重启及显式目录发现已有专项验证；这些结果不代替硬件发声、控制 p95 或 M05 的流加载/缓存验收。

旧 0.2.0 包的 [诊断](docs/diagnostics.json) 和 [验收记录](docs/validation.md) 不代表当前源码能力验收；测试命令和范围见 [后端状态](docs/backend-status.md#验证范围)及[SDK 状态](docs/sdk-status.md#验证范围)。

原创演示音频和测试源只访问工程自身的合法合成数据。项目许可与上游保留材料见 [LICENSE](LICENSE)、[NOTICE](NOTICE) 和 [上游许可](licenses/upstream/)。

验证记录：[alpha.14 后端检查点](docs/backend-checkpoint-alpha14.md)；历史记录：[alpha.13](docs/backend-checkpoint-alpha13.md)、[alpha.12](docs/backend-checkpoint-alpha12.md)、[alpha.11](docs/backend-checkpoint-alpha11.md)。

音频设备选择、静音、拓扑暂停与实际端点查询见[设备接口](docs/audio-output.md)。目标偏好与实际端点分别表示，Memory明确报告无物理设备；设备变更保持原媒体与缓存，实体端点验收独立执行。

[均衡器接口](docs/equalizer.md)提供固定十段增益、九个LX预设与平直复位。实际PCM系数按输出采样率准备，参数变化采用有限双滤波器过渡，设备变化重建系数并保持媒体身份；倍速与独立移调见[播放处理接口](docs/playback-processing.md)，卷积、空间声、压缩器、分析器和系统媒体继续实施。

[本地曲库接口](docs/local-library.md)记录目录与显式文件授权、健康状态、分页扫描、取消、监听、重定位和真实关停。目录撤销保留用户列表和歌词数据；标签完整冷恢复及大库运行性能继续验收。
