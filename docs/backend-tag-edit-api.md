# Backend 本地标签编辑协议

适用版本：`0.3.0-alpha.14`，Repository schema `16`。原生写入支持 Windows 的 MP3、FLAC；前端使用同一类型化 Backend 协议。底层授权、文件证明和提交事务见[本地标签安装与 Repository 接口](backend-tag-edit.md)。

默认启动只读取待恢复账本，不打开音乐文件、不生成写许可、不启动标签写入。读取、批准、编辑及恢复均由显式命令触发。文件散列、标签解析、写入和实际资源退休在有界工作线程执行。

## 命令

命令使用 `CommandEnvelope` 的 `request_id`、`origin`、可选 `expected_revision` 和 `command`。CLI JSON 将命令字段与信封字段放在同一个对象中；`command` 使用下表的蛇形名称。

| Backend 命令 / JSON 名称 | 参数 | 行为 |
| --- | --- | --- |
| `ReadLocalTags` / `read_local_tags` | `asset_id`, `tag_operation_id` | 使用已有本地读取授权，读取实际文件身份、完整 body SHA-256 和标签；返回后续明确选择所需的事实。 |
| `ApproveTagWrite` / `approve_tag_write` | `approval_id`, `selection` | 独立登记标签写许可，绑定实际选择与当前读取授权。 |
| `RevokeTagWrite` / `revoke_tag_write` | `approval_id`, `expected_approval_epoch` | 按许可 epoch 撤销写权。 |
| `ApproveTagRecovery` / `approve_tag_recovery` | `approval_id`, `tag_operation_id`, `selection` | 为指定旧操作登记新的明确恢复许可；选择必须来自当前实际文件读取。 |
| `EditLocalTags` / `edit_local_tags` | `approval_id`, `expected_approval_epoch`, `budget`, `patch` | 登记完整编辑意图，取得实际独占 writer，安装标签并处理资源退休。 |
| `RecoverTagEdit` / `recover_tag_edit` | `tag_operation_id`, `action`，可选 `approval_id`, `expected_approval_epoch` | 重观察并处理同一保留所有者；进程重启后的恢复必须同时提供新的恢复许可 ID 和 epoch。 |
| `CancelTagEdit` / `cancel_tag_edit` | `tag_operation_id` | 对匹配操作发出取消请求；回滚、发布和安全退休继续保持实际所有者。 |

前六项命令在工作线程成功准入后返回 `Accepted`。取消命令同步应用取消标志，通常返回 `Applied`；它不证明目标工作线程已经退出。

### 读取与写入选择

`ReadLocalTags` 的结果包含 `selection`、`size_bytes`、`modified_ms`、`title`、`artists`、`album`、`lyrics`、`picture_count` 和 `metadata_error`。标签解析错误可以独立出现在 `metadata_error`；实际读取通过时，文件身份和完整 body SHA-256 仍可用于明确恢复选择。

`selection` 是 `TagWriteSelection`，包含：

| 字段 | 绑定 |
| --- | --- |
| `operation_id` | 标签编辑的持久操作 ID |
| `asset_id` | 已有本地资产 ID |
| `path`, `expected_identity` | 当前实际路径、volume serial 与 file index |
| `root_id`, `grant_epoch` | 当前目录读取授权或精确文件读取授权 |
| `expected_binding_revision` | 当前资产绑定版本 |
| `expected_body_sha256` | 完整文件 SHA-256 |

调用方应原样保留读取结果中的 `selection`，再显式提交 `ApproveTagWrite` 或 `ApproveTagRecovery`。读取授权、文件导入、扫描结果、读取命令的 request ID 和历史安装回执均不提供写许可。

普通写许可要求当前内容证据与选择一致。部分写入后尚未提交的恢复允许当前 body SHA 与旧缓存内容不同；恢复 writer 在新取得的实际独占句柄上重新计算并匹配新许可的 SHA，随后才可恢复或发布。已提交、已恢复的操作还须匹配其持久回执。

## 查询

`BackendQueryResult` 的 JSON 使用 `kind` 与 `data`；`data` 可以为 `null`。读取结果的 request ID 与持久标签操作 ID 是两个独立标识。

| Backend 查询 / JSON 名称 | 参数 | 返回 |
| --- | --- | --- |
| `LocalTags` / `local_tags` | `operation_id`：读取命令的 request ID | `LocalTagReadView` 或 `null`；内存保留最近 64 次读取结果。 |
| `TagWriteApproval` / `tag_write_approval` | `approval_id` | 独立写许可、epoch、撤销状态与用途，或 `null`。 |
| `TagEdit` / `tag_edit` | `operation_id`：`selection.operation_id` | 指定持久标签操作的 `TagEditView` 或 `null`。 |
| `TagEdits` / `tag_edits` | 无 | 当前内存记录与持久待恢复记录对应的操作视图。 |

`TagEditView` 包含 `operation`、`running`、`has_live_owner`、`pending_recovery`、`finished`、`error`。运行期间 `has_live_owner` 保守表示实际所有者可能仍在工作线程中；工作线程真实退出后才报告退休状态。

## Patch 语义与幂等

`patch` 仅接受 `title`、`artists`、`album`、`lyrics`、`cover`，未提供的字段默认为 `keep`。未知字段会被拒绝。

| 值 | 语义 |
| --- | --- |
| `{"action":"keep"}` | 保留该字段与现有内容。 |
| `{"action":"remove"}` | 删除该字段。 |
| `{"action":"set","value":...}` | 写入提供的完整值。 |

`title`、`album`、`lyrics` 的值为字符串；`artists` 为字符串数组；`cover` 为 JPEG 或 PNG 的原始字节数组，不接受远程 URL。封面字节在工作线程中进行真实图片校验。`set` 与 `remove` 是不同编辑意图，空值不自动转换成删除。

标题、专辑及每个歌手最多 64 KiB，歌手最多 64 个，文本总量最多 16 MiB，文本不允许 NUL。封面最多 8 MiB，原音乐文件读取最多 1 GiB。CLI 的整个 JSON 请求仍受 2 MiB 限制；字节数组编码计入该请求长度。

Backend 根据版本标识、所有字段的 Keep／Set／Remove 语义及每一个封面字节生成 SHA-256。Repository 再将许可、许可 epoch、选择、预算和该摘要合并为完整请求摘要。同一持久操作 ID 和相同请求可以重放既有结果；同一 ID 配合不同 patch、预算或许可会失败。重放不会取得第二个独占 writer。

## 完成、额度与实际所有者

`Accepted` 仅表示已经准入。`OperationFinished.operation_id` 使用该次命令的 request ID，并在工作线程实际 join 后发布。它表示该次尝试结束；失败尝试可能已提交标签或仍有恢复义务，调用方须读取 `tag_edit` 判断持久状态。

`TagEditView.finished` 同时要求持久账本 `operation.is_finished()` 为真、没有运行中的工作线程、没有实际句柄和退休义务。`Published` 或 `Restored` 阶段本身不满足完成条件。

`budget` 必须包含 `original_bytes`、`journal_bytes`、`backup_bytes`、`candidate_bytes`。原对象预算不小于 candidate 预算；backup 与 candidate 均须容纳原文件；journal 最大 65 MiB；其余单项最大 1 GiB + 16 MiB。每个 Repository 最多 4 个未退休标签操作，原对象与三个工作文件的合计额度最多 4 GiB。Install 许可、Recovery 许可和操作各最多 64 条，实际账本文档与未使用预留合计最多 16 MiB。

Backend 的有界工作准入还计入尚未退休的实际标签所有者。原文件无法取得独占 writer 时返回 `Busy`，不强制停止已有读取者。SQL 观察失败不会丢弃已经取得的 native 所有者或其已登记 token。

新操作或尚未完成的操作要求对应 track 处于空闲。相同 track 的准备、待提升、预加载 Resolving／AwaitingApproval／Preparing／Ready，或音频 Loading／Buffering／Playing／Paused 均在 worker 与额度登记前返回 `Busy`；必须先显式 Stop。已经完成且完整意图相同的重放可以在播放中返回已有结果，不建立 writer，不改变播放 generation 或位置。

成功重放还要求持久 `Published` 阶段及实际 commit 回执。退休已经结束但没有 commit 的失败意图返回 `Conflict`，继续编辑须使用新的 operation ID，不能把原生 Busy、取消或回滚的资源退休完成当作标签安装成功。

实际空文件创建前先登记预算与 producer 账本；分配、封存、原始句柄关闭、工作文件删除和关闭分别提供实际证明，随后才更新相应额度 ACK。路径缺失、取消标志、普通析构、成功回执或阶段变化均不能直接释放额度。`Repository::tag_edit_quota()` 返回待退休操作数量与收费字节数；Backend 操作视图保留每项预算和 artifact 账本。

原 metadata 的持久编码预算在 journal 创建前检查，candidate metadata 在原文件写入前检查。每条完整 journal JSON 最大 1 MiB，成功及强制回滚的全部后续记录必须适配已批准 journal 预算；完整 Repository 操作文档含提交、恢复及退休回执，最大 1 MiB。原 metadata 阶段的提前拒绝保持全部原字节、修改时间和 binding 不变；candidate 阶段的拒绝沿回滚事务收束，在实际句柄退休后释放额度。每个活动操作登记时预留 1 MiB 操作文档、三个角色各 64 KiB 文档和 64 KiB 未来明确恢复许可；编码增长消耗原有预留。历史文档须先事务接纳预算，预算不足不打开文件且不改变已有账本或许可。

新 profile 的 `tag-edits` 目录由有界 worker 在当前审批、预算和 StoreFreeze 检查后创建。创建使用当前同一 ProfileGuard 的实际根目录 pin 与相对 no-follow 操作，拒绝目录替换及 reparse 路径；该 pin 保持到安装器建立自己的实际命名空间所有者。目录创建失败发生在 producer 额度登记前。

## 恢复与取消

`RecoverTagEdit.action` 接受：

| 值 | 动作 |
| --- | --- |
| `resume_publication` | 对已经提交或允许继续发布的同一操作完成发布。 |
| `restore_original` | 在未提交情况下，使用已有实际 backup 及同对象证明恢复原 body。 |
| `retire_artifacts` | 处理已达到安全阶段的原对象与工作文件退休义务。 |

进程内恢复继续使用同一实际所有者与 token。进程重启只恢复 SQL 待办列表；打开音乐和 journal 之前，必须用当前读取结果申请新的 `ApproveTagRecovery`，并将许可 ID 与 epoch 成对传入恢复命令。旧 journal、JSON 选择和历史回执本身不构成写能力。

Recovery 使用独立的 64 槽额度，活动操作已经预留一个槽及 64 KiB 文档空间；64 份 Install 许可不会占用该恢复槽。预留不创建批准。每个操作最多一份当前 Recovery 许可；新的明确批准可将已撤销或选择过期的旧许可归档为 revoked，完整历史最多 64 份且 64 KiB。全部原对象与工作文件的实际退休 ACK 完成时，当前恢复行在同一事务归档并回收槽位；`tag_write_approval` 对该已回收 ID 返回 null，`tag_edit.operation.recovery_approval_history` 保留撤销历史。归档 ID 不得复用。

提交前的取消或写入错误可以触发安全回滚；已提交的操作继续发布并退休，不使用旧 backup 覆盖已提交标签。取消普通编辑继续条件不会撤销实际所有者的安全回滚与退休责任。关闭、profile quiesce 和 actor 失败保留同一 Runtime，直到标签工作线程与实际所有者完成真实退休；待冷恢复的持久记录可以继续保留，正常关闭会报告其未完成状态。

工作文件已经实际删除／关闭、但退休 ACK 尚未完成时，新的明确恢复许可允许对合法 `Retiring` 账本执行独立退休。安装器保持实际目录 pin，重新完成同卷 NTFS 删除与关闭校准，并以正向目录控制核对确切 FileID 的 8／16 字节缺失结果，再生成 opaque witness 提交原 ACK。路径缺失不能退款；同名的不同 FID 文件保留。此路径要求原句柄关闭 ACK 已提交，处理时不重新打开原音乐文件，也不要求已经退休的 journal 仍存在。

操作预算登记后、Coordinator 初始化前，Backend 保留同一 `unstarted` 所有者。第一项原对象 native open 前，Repository 将 `installation_attempted` CAS 为 `Some(true)`。只有该字段明确为 `Some(false)` 且全部角色明确从未分配时，冷恢复才可在新的恢复许可下提交 unopened／unallocated ACK。已经尝试或旧 schema 16 字段缺失的状态继续保留额度，不能使用另造占位所有者退款。

原对象关闭前持久登记同旧 writer 验证的 `original_retirement_intent`。已经完成 `Published`／`Restored`、但关闭 ACK 尚未提交的操作，可用新的明确恢复许可校准实际同对象双向零共享准入：新的 RW 控制存活时只读准入必须 Busy，明确关闭控制后只读准入成功，只读存活时 RW 准入必须 Busy。文件身份、单链接、NTFS 卷、纳秒时间戳和完整 SHA 均须匹配 intent。

新的零共享只读句柄实际关闭后，重新取得同 FID 的 `READ_DATA`／`shareREAD` 稳定读取 lease，并在其禁止数据写入、DELETE 准入及重命名的保护下重新完整核对 SHA、路径、FID、卷和时间戳；lease 保持至 SQL ACK。独立 `original_recovery` 回执记录旧排他 owner 无法继续存活的准入证明，保留 `original_close=null`；不会将新句柄的关闭结果写成旧 native close witness。ACK 后明确关闭 lease 及新命名空间所有者，再退休工作文件。缺失旧 intent、Busy、外国 FID、文件内容改变或未知校准语义时不释放额度。调用方保留操作 ID、错误和收费状态，不将路径缺失或阶段值单独显示成完成。

## CLI stdio

使用 `lx-native-cli --data-dir PATH --audio memory --json-lines` 保持一个 Backend 实例，每行输入一个 JSON 对象。stdio 返回 `Accepted` 时，随后通过 `events` 查询观察对应 request ID 的 `operation_finished`，再查询持久操作；输入读取结束触发正常关闭。

```json
{"request_id":"read-001","command":"read_local_tags","asset_id":"asset-001","tag_operation_id":"edit-001"}
{"query":"local_tags","operation_id":"read-001"}
{"query":"tag_write_approval","approval_id":"write-001"}
{"query":"tag_edit","operation_id":"edit-001"}
{"query":"tag_edits"}
```

批准命令的 `selection` 使用 `local_tags.data.selection` 的实际返回值。编辑请求示例：

```json
{"request_id":"write-001-attempt","command":"edit_local_tags","approval_id":"write-001","expected_approval_epoch":1,"budget":{"original_bytes":1048576,"journal_bytes":2097152,"backup_bytes":1048576,"candidate_bytes":1048576},"patch":{"title":{"action":"set","value":"歌曲标题"},"artists":{"action":"set","value":["歌手"]},"album":{"action":"keep"},"lyrics":{"action":"remove"},"cover":{"action":"keep"}}}
```

示例中的许可 epoch 与预算须使用当前实际许可及文件大小。该请求不会自动创建许可。

单次 `--command JSON` 模式对 `Accepted` 等待实际 `OperationFinished`，并返回 `receipt`、`completed`、`success`、`error`、`tag_result`。`--json-lines` 返回接收结果与当时的 `tag_result`，不隐式等待完成。快捷查询为 `local-tags READ_REQUEST_ID`、`tag-write-approval APPROVAL_ID`、`tag-edit TAG_OPERATION_ID`、`tag-edits`；`read-local-tags ASSET_ID TAG_OPERATION_ID` 生成独立读取 request ID。

## 验证范围

协议的类型、全部命令及查询路由已通过 headless 元数据编译检查。CLI 路由、能力声明与不完整明确许可拒绝已有实际测试；原生文件退休的五个定向用例已通过。

实际标签集成验证通过 39 项：公开 Backend 流程 3 项、安装器 13 项、Repository 23 项；另有原生文件退休 5 项通过，包括稳定 READ_DATA lease 的写入、DELETE、重命名拒绝及实际新控制句柄关闭失败保留。安装器包含真实子进程执行的 8 个阶段故障场景；子进程辅助入口不单独作为普通测试运行。

公开 Backend 用例覆盖新 profile 自动创建暂存目录、读取不授写、独立许可、MP3 实际标签与编码音频 payload 不变、同 FID、同操作不同 patch 拒绝、实际 Playing／Paused 的写入拒绝、Stop 后编辑，以及播放中完整完成回执的准确重放与 generation 保持。真实 OS 读取者导致的失败意图在退休后重放返回 Conflict，关闭读取者后可用新意图完成编辑。Repository 的真实 700 KiB 引号歌词用例验证原写入前拒绝 JSON 预算超限，全部原字节、纳秒时间戳、binding 和实际收费保持正确。冷恢复另覆盖三个角色删除后 ACK 故障、Coordinator SQL 初始化失败、旧尝试字段未知，以及原对象持久关闭 intent、独立恢复 ACK 故障、实际 Busy 和文件改变拒绝。历史 intent 缺失继续保留实际额度。

Repository 容量验证还覆盖 15 个真实 900 KiB 元数据操作后的四个并发准入、16 MiB 全局未来文档预留、旧记录接纳拒绝的全部账本及授权回滚，以及 64 个 Install 许可已占满时实际退出／重启后的明确 Recovery 准入与终态槽回收。
