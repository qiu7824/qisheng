# LX Native 0.3.0-alpha.3 后端检查点

日期：2026-10-06。平台：Windows x64/MSVC，Rust/Cargo 1.99.0，数据库 schema 5，后端协议 1。

## 可运行范围

alpha.2 的独立后端、五平台 SDK、事务仓、源工具及原生歌词 codec 延续保留。列表、收藏、Dislike、搜索历史与五模式队列已接入后端和 JSON CLI，支持独立列表副本、临时 FIFO、预选/历史、显式质量、当前项删除、受限错误跳过、取消和持久恢复点。

在线 SDK 结果可以显式保存至列表，通过已批准音源解析并经 URL 审批进入音频与内置 SDK 歌词链路。音源 legacy musicInfo 使用当前列表记录的 metadata 和规范质量；原格式导出的 absent/null、未知字段与历史质量名保持归档语义。重启恢复暂停意图与位置，首次显式控制按需初始化源。

列表工具提供明确 locale 的排序、重复候选及 UTF-16 分级搜索；SQL 查询按页解码列表记录。目录模块提供显式根下的有界文件发现、预算与取消，不负责目录自动入库或 roots 索引。

## 验证证据

| 检查 | 结果与范围 |
| --- | --- |
| 无界面统一回归 | 37 个套件，342 项通过，0 项失败，1 项物理输出设备测试跳过 |
| 实际列表/播放链路 | SDK HTTP → default/love 独立副本 → 实际 SourceWorker → URL 审批 → Memory PCM 与在线歌词；未批准前媒体请求为零 |
| 控制及恢复 | 显式质量无静默降级；解析失败期间暂停、延迟错误跳过取消；当前项/整列表删除、旧 generation 失效；重启不执行源或下载；正常结束与手动 Next 区分 |
| metadata 与质量 | domain 与实际 worker 验证新 hash/media ID/未知字段、32→24 请求质量及列表隔离；原格式 roundtrip 保持 |
| 纯队列与事务 | 22 项纯队列、9 项列表仓、7 项 checkpoint 测试通过；50,000 个稳定 ID 真实存储/关闭/重开，保持全部顺序 |
| 列表工具与目录 | 独立 Node locale/排序/重复/搜索 golden 与 1505 条分页；目录发现 4 项集成与 JSON 预算单位测试。Unix 专有路径/链接测试未在 Windows 执行 |
| 静态检查 | 格式检查、无界面全部目标严格 Clippy、默认 GPUI 特性全部目标编译检查通过 |
| 正式路径 CLI | 版本、能力、列表查询、独立 worker、实际 Memory PCM、暂停 checkpoint 与 lazy 重启烟测通过 |

统一 Cargo 总数不重复累加专项、Node 差分及 smoke。50,000 条 checkpoint 证明存储容量与恢复，不能替代控制 p95 或完整大曲库性能验收。目录取消在同步文件系统操作之间协作检查，Windows reparse 的实际联测与硬件输出独立于上述结果。

当前检查点验收状态：通过。完整后端状态：未完成。

## 开发产物

文件：[lx-native-cli-0.3.0-alpha.3.exe](../bin/lx-native-cli-0.3.0-alpha.3.exe)。大小：38,599,168 bytes。

SHA256：`E62F8C843E815FE0BC8B674593D7968D58395D4FE5C6872867EF3E5E8A3B1307`。

CLI 默认 Memory，`--audio device` 明确选择真实输出。列表、队列、provider 命令和查询见 [README](../README.md)。schema 5 profile 由相应版本访问，旧开发二进制拒绝未知新 schema。

## 后续范围

M05 继续实现流式媒体、Range/表示缓存、预加载、换源匹配及 URL 失效处理，架构与验收见[流式播放设计](streaming-architecture.md)。当前播放 loader 仍为 64 MiB/20 秒。目录入库/索引、设备/DSP、下载与后处理、备份互通、LX v4 同步、OpenAPI/系统控制及完整发布继续按[实施规划](backend-port-plan.md)推进；现有前端保持适配，不新增界面设计。
