# 本地曲库接口

本地曲库通过显式文件授权、目录根授权、后台扫描和分页查询提供索引。读取范围由授权记录与实际文件身份共同确定；曲目、资产、文件位置和播放队列身份分别保存。

目录服务提供添加、移除、手动扫描、扫描取消、目录健康查询和资产分页。原生文件监听、文件重定位和标签写入尚未提供对应控制接口；`watch_enabled` 保持 `false`。

## 文件授权与目录授权

`import_audio` 接收明确选择的文件。成功导入在同一仓事务中保存曲目、资产、当前位置、实际文件身份及显式文件授权。授权仅覆盖所选音频和允许的精确同名旁车，不授予整个父目录。

`add_library_root` 接收绝对目录路径，`recursive` 默认 `true`。后台验证目录后保存实际目录 anchor 与授权 epoch；添加目录本身不扫描音频标签、不自动导入目录内文件。`scan_library_root` 才开始发现和入库。非递归授权只覆盖根目录直属文件。

同一资产可以同时具有显式文件授权及多个目录成员关系。有效授权按 OR 判断：移除一个目录后，其他有效目录或显式文件授权仍可支持读取。最后一个目录授权撤销后，相关目录路由和未完成读取停止使用；资产、曲目、用户列表、队列 identity 及已保存 raw/edited 歌词继续保留。

同一实际目录、相同递归策略的重复添加保留 `root_id` 和 `grant_epoch`。明确改变范围或重新批准同名的新目录对象会推进 epoch；旧扫描和旧读取不能借新 epoch 恢复。缩小递归范围同时撤销旧范围下的授权，并根据剩余授权重新建立可用路由。移除的根保留内部 tombstone，重新添加不会重用旧有效 epoch。

## 命令和查询

命令使用现有 `CommandEnvelope`：`request_id`、可选 `origin`、可选 `expected_revision` 与扁平 `command`。异步添加、扫描首先返回 `accepted` receipt；`operation_id` 用于查询状态和关联事件。移除目录是仓持久化操作。

| 命令 | 参数 | 行为 |
| --- | --- | --- |
| `import_audio` | `paths` | 明确导入 1–1,000 个文件；显式授权与资产整批提交 |
| `add_library_root` | `path`、`recursive` | 验证实际目录并持久化授权，不开始扫描 |
| `remove_library_root` | `root_id` | 撤销该根授权，取消相关扫描，保留用户数据 |
| `scan_library_root` | `root_id`、可选 `options` | 执行有界发现、元数据准备、分批入库和完整范围的缺失处理 |
| `cancel_operation` | `operation_id` | 协作取消对应操作；已提交批次保留，未提交批次回滚 |

查询返回 `BackendQueryResult` 的 `{"kind":"…","data":…}` 结构。

| 查询 | 参数 | 返回 |
| --- | --- | --- |
| `library_roots` | `offset`、`limit` | `items/total/offset/limit/revision`，只列活动根 |
| `library_root` | `root_id` | `LibraryRoot` 或 `null` |
| `library_root_health` | `root_id` | `root_id/grant_epoch/health` 或 `null`，不触发文件 I/O |
| `library_operations` | `offset`、`limit` | 合并运行时根验证和仓中扫描状态的有界操作页 |
| `library_operation` | `operation_id` | 操作状态或 `null`；仓中终态优先于运行时旧进度 |
| `library_assets` | 可选 `root_id`、`after_asset_id`、`expected_revision`，以及 `limit` | `items/total/next_asset_id/revision` |

分页 `limit` 默认 100，允许 1–1,000。资产页按 `asset.id` 排序，使用 keyset；响应达到 8 MiB 字节预算时提前结束并返回 `next_asset_id`。下一页将该值作为 `after_asset_id`，将第一页的 `revision` 作为 `expected_revision`。仓 revision 改变时返回 `conflict`，调用方重新开始分页。授权健康可以在不改变仓 revision 的情况下变化，`authorized` 是查询时的有效状态。

根和操作页采用 offset。操作页按 `operation_id` 排序，保留最多 64 个合并状态；它不是无限历史日志。扫描运行和终态进入 SQLite；单次添加根的验证状态由当前进程保存。页响应超出字节预算时应降低 `limit`。

`LibraryRoot` 字段为 `root_id/path/recursive/grant_epoch/anchor/path_policy/watch_enabled/generation/last_complete_generation`。`anchor` 为 `FileIdentity { kind, volume, file_id }`。客户端原样使用返回的 identity，不从显示名称或路径拼接 ID。

`LibraryAssetView` 包含 `asset`、`authorized`、`root_ids` 和 `explicit_file_grant`。`asset.available` 是已索引文件的物理可用记录；`authorized` 表示当前可生效的授权，两者不等价。目录的持久成员关系不会自动升级为显式文件授权。

状态快照的 `library` 摘要包含 `roots/authorized_assets/running_operations/latest_operation_id/revision`。目录计数表示活动授权根；有效资产计数包括显式文件授权，以及物理验证成功且 epoch 匹配的目录授权，同一资产只计一次。

## CLI JSON 会话

目录控制使用 JSON 命令接口。长扫描采用持续会话，查询 operation 或订阅事件；`--json-lines` 不等待每个 accepted 操作完成。

```powershell
lx-native-cli --data-dir C:\LXNativeData --audio memory --json-lines
```

每行提交一个 JSON 对象。下列 `<root_id>` 和 `<operation_id>` 分别替换为根查询和命令 receipt 返回的原始值。

```json
{"request_id":"root-add-1","command":"add_library_root","path":"C:\\Music\\Library","recursive":true}
{"query":"library_operation","operation_id":"root-add-1"}
{"query":"library_roots","offset":0,"limit":100}
{"query":"library_root_health","root_id":"<root_id>"}
{"request_id":"scan-1","command":"scan_library_root","root_id":"<root_id>","options":{"timeout_ms":120000}}
{"query":"library_operation","operation_id":"scan-1"}
{"query":"library_assets","root_id":"<root_id>","limit":100}
{"request_id":"cancel-scan-1","command":"cancel_operation","operation_id":"scan-1"}
{"request_id":"root-remove-1","command":"remove_library_root","root_id":"<root_id>"}
```

`--command JSON` 是一次性入口；非 provider 的 accepted 操作等待上限为 80 秒，进程退出时请求后端关停。超过这个等待范围的扫描使用持续 JSON 会话。`options` 可省略，缺失字段按默认值补齐，未知字段被拒绝。

## 扫描状态与事件

`LibraryOperationView` 保存 `operation_id/root_id/generation/state/phase`、`entries_examined/discovered_files/prepared_files/committed_files/skipped_files/changed_files/missing_files`、`warnings/omitted_warnings/error/repository_revision`。

| `state` | 含义 |
| --- | --- |
| `running` | 验证、发现、准备或提交仍在进行 |
| `completed` | 授权范围发现完整并完成终态处理 |
| `partial` | 只取得部分结果，或完整发现中存在准备失败；查看警告和已提交计数 |
| `cancelled` | 操作或相关授权已取消，未提交数据不能继续入库 |
| `failed` | 预算、存储、文件访问或处理失败 |
| `interrupted` | 上次进程退出时扫描尚未完成，恢复时标记中断 |

`phase` 为 `validating_root/discovering/preparing/committing/finalizing`。进度最多每 100 ms 发布一次；事件包括 `library_root_changed { root_id, removed }`、`library_scan_progress { state }` 和 `library_scan_finished { state }`，同时沿用通用 `operation_finished` 完成协议。

计数中的 committed/changed/missing 由实际仓提交累计，不能用准备完成数代替。`changed_files` 包含索引元数据变化，不意味着音频表示一定变化。只修改标题、作者、专辑或增强同一物理文件身份的证据不会据此停止音频；路径、大小、修改时间及已有实际身份变化按媒体表示变化处理。

错误类型为 `invalid_data/conflict/busy/cancelled/timeout/storage/unavailable/budget_exceeded`；后端将预算错误映射到受限准入的 `busy` 类型。同步文件 I/O 的取消和 deadline 是协作检查，调用方的等待超时不表示系统 I/O 已结束。

## 目录健康与实际身份

| `health` | 读取条件 |
| --- | --- |
| `unknown` | 只有持久授权意图，尚无当前进程验证的目录 proof |
| `verified` | 实际目录 anchor 匹配，并保留验证所需目录句柄 |
| `unavailable` | 目录暂不可访问；授权意图保留，可对相同 anchor 重试 |
| `needs_reapproval` | 同一路径对应的实际目录身份改变，要求明确重新添加授权 |

启动按持久索引建立惰性路由，并通过有界后台任务验证目录；不在控制线程遍历整个曲库标签。仅安装 intent 不产生 `verified` 状态。健康更新不写用户 revision；旧 epoch 的验证结果不能使新根生效。

Windows anchor 使用系统实际文件身份，打开目录和文件时排除嵌套 reparse，并核查实际父目录句柄包含所选 root anchor。根选择可以解析到明确目标，之后的嵌套链接不能扩大范围。大小写敏感目录返回明确不支持结果，避免与现有路径 identity 规则冲突。

Unicode `to_lowercase` 不能证明两个路径是同一文件。例如 `İ` 与 `i` 加组合点可以具有相同文字 key，却对应不同实际目录。不同 literal 规范路径只有旧、新实际 `FileIdentity` 都存在且相同时才能复用资产；弱旧证据或不同实际身份返回 `conflict`，整批回滚。根 key 碰撞且实际 anchor 不同时同样拒绝。明确选择同一 literal 路径下的新文件表示可更新其实际身份。

安全打开实现包含 Windows 和 Linux 分支。Linux 使用目录描述符、`openat` 与 no-follow 检查；其他平台返回明确不支持结果。平台发行与验收范围在[后端状态](backend-status.md)单独列明，存在 Linux 安全打开分支不表示 Linux 发行验收已完成。

## 标签、旁车与媒体时钟

解码探测、标签读取、歌词和本地封面通过已验证、可撤销的打开文件读取。克隆 reader 具有独立逻辑游标，metadata 探测和音频读取不竞争共享 seek 位置。每次有界 read 检查授权、取消及实际文件身份；旁车同时保留原音频授权关联。

探测真实解码的首个有效样本。解码 probe 的实际累计读取最多 16 MiB，单次 read 最多 64 KiB；seek 不消耗或恢复读取预算，可跳过大型 WAV 数据段或访问 MP4 尾部。预算用尽返回 `LocalProbeBudgetExceeded`，即使解码器吞掉底层 I/O 错误也不会当作成功或假 EOF。

标签解析独立限制累计读取和分配。标签读取失败但 PCM 可探测的已有资产保留既有标题、艺术家和专辑；新时长未知时保留已有时长。路径、文件大小、修改时间、实际 FileIdentity、授权和 URI 使用新准备的实际文件值。新文件标签缺失或损坏采用文件名等回退，并保存警告。

`LocalAsset.fingerprint` 在实际身份已知时记录 FileIdentity JSON 的 SHA-256，用于身份与媒体 stamp；它不是音频内容 SHA-256。文件位置、物理身份和标签文本不用于重算既有 track/asset identity。

歌词顺序为精确同名 `.lrc`、`.krc`、内嵌。允许 UTF-8、UTF-16LE/BE 和 GBK 策略，旁车最大 2 MiB；四轨 raw/edited 和时钟接口见[后端接口](backend-status.md)。只读取允许扩展名的精确同名旁车，不自动授予其他目录。

## 仓迁移与提交

SQLite schema 8 保存 `library_roots/local_file_grants/local_asset_locations/library_membership/library_scan_runs/library_scan_seen`。迁移使用既有独占 profile 锁、预备份、版本与应用 identity 检查、同事务 DDL、完整性及 foreign-key 检查。未知更高 schema 或非法旧身份数据拒绝覆盖；迁移失败回滚并保留原数据与预备份。

schema 7 已注册的 local assets 一次性保留为显式文件授权；该过程只读取和规范化已有仓文档，不额外扫描目录。新目录资产只保存 membership。启动恢复不把所有资产路径回填为 `ProjectStore.local_files`，从而不会在重启后恢复已撤销的目录读取权限。

扫描 token 同时绑定 `root_id/grant_epoch/generation/operation_id`。每批提交在事务前、逐项和实际 commit 前检查取消与 token，track/asset/location/membership/seen、revision 和 outbox 一起提交或回滚。仓成功后才发布 Catalog 项；每 worker 等待一次 ACK 才继续下一批。

发现阶段先分批登记 seen，再准备标签和媒体。已发现但准备失败的旧文件仍有 seen，不会仅因坏标签被判作 missing。仅完整发现范围、有效 token、未取消和未超时的终态允许按未 seen 成员处理缺失；不完整枚举、深度限制、预算失败和取消不能把未访问成员批量标缺失。完整处理将缺失资产的 `available` 置为 `false`，保留相关 identity、列表和编辑歌词。

中断扫描恢复为 `interrupted`，清理自身 seen 暂存，不以中断清单推断缺失。操作记录采用有界保留；已提交批次与其 outbox 不因之后取消而撤销。

## 资源边界

| 项目 | 上限或默认值 |
| --- | --- |
| 活动根 / 资产 / membership | 128 / 50,000 / 200,000 |
| 本地 track 与 asset 编码元数据总量 | 64 MiB |
| 根文档 / 操作文档 | 各 8 MiB |
| 操作保留 | 最多 64；字节限额可提前淘汰终态 |
| 请求队列 / profile 文件 worker / completion 队列 | 8 / 2 / 2；同根最多一个扫描 |
| 扫描批次 | seen 和 prepared 各最多 200 项，总编码最多 1 MiB |
| 准备存储预约 | 每批共享预约 8 MiB，进程合计最多 32 MiB |
| 授权与 proof 存储预算 | 进程合计最多 32 MiB |
| 目录句柄退休槽 | 最多 2,048，实际打开前预占 |
| 文件 / 条目 / 目录 / 深度 | 默认 50,000 / 250,000 / 10,000 / 64 |
| 发现报告 / 待处理目录路径 | 各 8 MiB |
| 扫描 timeout | 默认 120,000 ms，允许 1–600,000 ms |
| 警告 | 最多 256；操作警告编码总量最多 256 KiB，reason 最多 512 字符 |
| 路径 | 绝对、无 NUL、无损 UTF-8，最多 16 KiB |
| 显式导入批次 | 1–1,000 项，持有元数据与仓编码均受 32 MiB 限制 |
| 标签 reader / 单图 / 总图 / 图片数 | 16 MiB / 8 MiB / 16 MiB / 16 |

条目数量和驻留字节分别约束请求；数量上限不替代字节准入。扫描采用有界流式枚举，不把单目录所有 `DirEntry` 或整库音频读入内存。控制循环每轮处理有限 completion，避免持续扫描占满命令处理。

## 关停契约

profile 关停关闭新请求准入、撤销运行时读取、取消未完成作业、清理缓存 proof 引用并 drain 待处理 ACK。成功条件同时包括两个 profile 文件 worker 的实际退出/join，以及该 profile 的目录和文件句柄在真实关闭后 `retirement_pending == 0`。取消标志、线程已收到唤醒、缓存引用已清空都不能代替实际关闭完成。超时保留 owner 和未完成资源，允许等待或重试；worker panic 返回失败。

最后一个目录 proof 的 Drop 将已经预占退休槽的资源交给 `LibraryRetirement` supervisor；实际驱动关闭和内存释放完成后才释放 scope 计数、存储预算及槽位。控制线程不在最后 Drop 中执行目录 `CloseHandle`。profile 的新预约与 closing 使用共同原子准入状态，关停之后不能再成功预约新 proof。

`lx-library-fd-owner` 是有注册 owner 的进程期退休 supervisor，与两个 profile 文件 worker 分开；profile 完成关停时它可以继续空闲等待其他 profile/独立 reader 的退休工作。profile 关停判定针对本 profile 的真实 join 与 pending 清零，不能把该进程期线程的空闲状态当作仍有未关闭 profile 句柄，也不能用它的存在掩盖未 join 的 profile worker。

目录服务的最终发行验收还覆盖取消 ACK、最后句柄关闭、异常终止恢复和资源压力边界。监听、重定位及标签写入保留为独立功能范围。
