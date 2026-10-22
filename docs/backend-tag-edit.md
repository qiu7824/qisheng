# 本地标签编辑接口

适用版本：`0.3.0-alpha.14`，Repository schema `16`。

Windows 的 MP3／FLAC 标签安装器与 Repository 提供独立写授权、同对象文件证据、事务提交和实际退休接口。Backend 与 CLI 协议见[标签编辑 API](backend-tag-edit-api.md)，前端编辑界面尚未接线。

## 独立写许可

`approve_tag_write(approval_id, selection, check)` 显式登记 `TagWriteApproval`。本地库读取授权、目录扫描和文件导入不会自动生成标签写许可。

`TagWriteSelection` 绑定以下内容：

| 字段 | 约束 |
| --- | --- |
| `operation_id`、`asset_id` | 固定操作与已有本地资产 |
| `path`、`expected_identity` | 规范化路径与实际物理文件身份 |
| `root_id`、`grant_epoch` | 已有目录读取授权或精确文件读取授权的当前 epoch |
| `expected_binding_revision` | 当前资产绑定版本 |
| `expected_body_sha256` | 必填的完整原文件 SHA-256 |

首次批准检查资产可用性、路径、FID、绑定版本、读取授权及已有内容证据。相同 approval ID 和相同选择可以重放已有许可；已撤销许可或复用 ID 对应不同选择会被拒绝。

`revoke_tag_write(approval_id, expected_epoch, check)` 使用许可 epoch 撤销写权。后续编辑必须通过当前许可、读取授权及绑定检查；撤销写权不会新增、删除或续期读取授权。

## 操作登记与幂等

```rust
begin_tag_edit(
    approval_id,
    expected_approval_epoch,
    budget,
    input_digest,
    check,
)
```

`input_digest` 是调用方编辑意图的 SHA-256，以 64 个十六进制字符表示。调用方应覆盖完整标签输入，包括 Keep／Set／Remove 语义和封面内容摘要，并保持工作线程使用同一份不可变输入。

Repository 将 approval ID、approval epoch、选择、预算和 `input_digest` 合并为操作请求摘要，登记 `TagEditToken` 的 operation epoch。相同操作 ID 和相同摘要返回已有操作及回执，不增加 Repository revision；不同摘要复用操作 ID 会失败。重放已有操作不会准许另一个写句柄；新文件打开仍须通过 Coordinator 的当前意图检查。

schema 16 使用三个账本表：

- `tag_write_approvals`：独立写许可及撤销状态。
- `tag_edit_operations`：操作 token、阶段、账本 CAS、提交／回滚回执与原句柄退休状态。
- `tag_edit_artifacts`：journal、backup、candidate 的预留、实际分配身份、封存字节数和 SHA-256。

物理快照的权限清除流程删除这三个表中的记录，并验证写许可和恢复账本为空。导入备份的声明或历史回执不能恢复标签写权。

## 文件与额度预算

`TagEditBudget` 分别预留原始写对象、journal、backup 和 candidate 的字节数。每个 Repository 最多保留 4 个仍有退休义务的标签操作，所有原对象和工作文件的合计额度最多为 4 GiB。

| 预算项 | 上限及关系 |
| --- | --- |
| `original_bytes` | 最大 1 GiB + 16 MiB，必须不小于 candidate 预算 |
| `backup_bytes` | 最大 1 GiB + 16 MiB，必须容纳原文件 |
| `candidate_bytes` | 最大 1 GiB + 16 MiB，必须容纳初始原文件副本 |
| `journal_bytes` | 最大 65 MiB |
| 账本文档 | 实际文档与未使用预留合计最大 16 MiB；Install 许可、Recovery 许可、操作各最多 64 条 |

操作登记先完成额度检查和三个工作文件的预留，随后才能创建实际空文件。journal 创建前还检查原写句柄的实际长度是否超出 backup／candidate 预算；拒绝时返回持有实际写句柄的错误所有者。

`TagArtifactAllocation` 在同一空文件及目录句柄仍存活时确认实际 FID、路径和目录身份。`TagArtifactSeal` 确认同一文件的实际字节数、时间戳、FID 和完整 SHA-256。封存将记录更新为“实际字节 + 未使用预留”，总收费保持该文件的完整已批准预算。

额度记录不能因路径消失、阶段为 `Published`／`Restored` 或普通对象析构而直接归零。

持久记录预算在原文件写入前检查。`TagPersistenceBudget<'a>` 借用实际原 writer 与 candidate 句柄，并使用同句柄解析的元数据；预算估计不能构造提交证明。原 metadata 在 journal 创建前检查，candidate metadata 在 `Installing` 前检查。journal 的每条完整 JSON 记录最大 1 MiB，成功发布及强制回滚的全部后续记录、36 字节记录封装、当前实际长度和记录数量共同受已批准 `journal_bytes` 约束；回滚记录同时容纳 candidate 回执及原 metadata。

Repository 在真实事务中核对 token、当前许可与资产，再检查包含提交／回滚回执、关闭 intent、关闭回执及独立恢复回执的完整操作文档最大 1 MiB。将来的时间戳及 revision 只按数值编码上界预留；真正提交仍要求 live `TagPreparedAsset`。拒绝时保留实际句柄并执行明确关闭，不能伪造 unopened 证明。

全局账本文档的 16 MiB 上限同时核算实际文档和活动操作的未使用预留。操作登记预留 1 MiB 操作文档、三个工作文件各 64 KiB 账本及 64 KiB 的未来明确恢复许可文档；实际编码增长消耗相应预留，不增加合计收费。`tag_document_budget()` 返回实际、未使用预留和总收费字节数。旧文档缺少预算标记时，必须先在事务内完成预算接纳，再准许新的 native 打开；不足时保留原账本和授权。

Install 的 64 条额度独立于 Recovery 的 64 条额度。每个已接纳活动操作保留一个恢复许可槽，但预留本身不生成授权；新的明确恢复选择仍须通过当前读取授权、FID、绑定及操作 epoch 检查。每个操作最多保留一份当前恢复许可；已撤销或选择过期的许可可在明确新批准时归档为 revoked。完整历史最多 64 份且 64 KiB，其编码空间在原文件写入前计入 1 MiB 操作文档。全部实际退休 ACK 完成时，同一事务归档并删除当前 Recovery 行、回收槽位和未使用预留。归档 ID 不得复用，历史记录不提供写权；Install 记录保留。

## 同句柄证据与提交 CAS

`NativeWriterToken` 持有实际独占写句柄及命名空间证明。安装器在 backup 和 candidate 上准备并验证标签，保持编码音频 payload 的 SHA-256 不变，然后写入同一原对象并重新读取其标签和文件事实。

`TagPreparedAsset<'a>` 借用该独占句柄，不能由 JSON 或持久回执构造。其 `check()` 验证实际 FID、路径、链接状态、长度和高精度修改时间；`prepare_asset()` 保留原 track／asset ID 与未知 DTO 字段，并投影同句柄重新读取的标题、歌手、专辑及新文件信息。

`RepositoryTagEditCoordinator::new(repository, token, continue_check, owner_check)` 区分普通编辑继续条件和实际所有权检查。回滚可以处理已经取消的编辑，仍须验证实际所有者和 SQL 绑定。仅凭 `TagInstallationReceipt` DTO 调用本地提交或发布接口会被拒绝。

`TagOwnedInstallation::unstarted(selection)` 在 Coordinator 初始化前保留尚未打开的操作义务，随后由 `start_selected` 消费同一所有者。第一项原对象命名空间或文件打开之前，`before_installation_open` 将 `installation_attempted` 从 `Some(false)` CAS 为 `Some(true)`。旧 schema 16 文档缺少该字段时为 `None`，不能据此证明从未打开。新建未启动占位所有者不能退还已标记尝试过的操作额度。

`commit_tag_edit(token, prepared, check)` 在锁前、锁后和事务提交前检查 live proof，并在单个事务内完成：

1. 验证 operation token、当前独立写许可、读取授权、原绑定版本、原 FID 和原文件表示。
2. 验证 backup、candidate、journal 的实际封存账本；backup 匹配原 body SHA，candidate 匹配安装后 body SHA。
3. 更新 track／asset 标签与文件信息、内容 SHA 和时间戳，CAS 增加 binding revision。
4. 更新已有列表条目的元数据副本，清除旧重定位候选。
5. 写入 `LocalTagsEdited` outbox、提交回执和 `RepoCommitted` 阶段。涉及公共列表时，同步数据投影也在该事务内更新。

此路径不调用扫描 upsert，不补充或续期精确文件读取授权。撤销许可、改变 grant epoch、移动／替换文件或改变绑定版本均可拒绝过期操作。

## 回滚与待退休状态

DB 提交前发生实际写入失败时，安装器使用已封存 backup 恢复原文件。`restored_asset` 回调提供同一独占句柄重新验证的原 body SHA 和恢复后文件事实；`commit_tag_restoration` CAS 更新 asset／location 的修改时间、内容证据与 binding revision，并写入 `LocalTagsRestored` 回执。原 track／列表标签信息及读取授权保留。

DB 已提交的操作通过已有回执继续发布，不能恢复旧 body 覆盖已提交标签。恢复检查同时约束真实文件事实、当前 binding revision 和已提交 SHA；历史 journal 内容本身不构成授权。

阶段含义如下：

| 阶段 | 已证明的状态 |
| --- | --- |
| `DiskVerified` | 原写对象与已验证 candidate body 一致 |
| `RepoCommitted` | 标签、绑定、列表副本和 outbox 事务已经提交 |
| `Published` | 安装器发布回调与 journal 阶段完成 |
| `Restored` | 原 body 已恢复，同句柄回滚事实已经提交 |

`Published` 和 `Restored` 在完成全部退休 ACK 前仍属于 `pending_tag_edits()`。安装器保留同一 owned Session；原文件实际关闭后提交最终时间戳证明，工作文件通过同句柄 disposition、零链接、明确关闭及校准 FID 缺失观察生成 opaque witness。Repository 收到对应 ACK 后才逐项释放额度，所有义务清除后标记 `finished`。ACK 失败保留 witness，重试不再次删除；journal 最后退休。冷恢复的未闭合窗口见[标签编辑 API](backend-tag-edit-api.md)。

原句柄关闭 ACK 已提交的操作可单独恢复剩余工作文件退休。Repository 提供 `TagRetirementRecovery` 的封存 FID、目录 FID、字节数及 SHA；这些字段只标识义务。安装器重新持有实际目录 pin，创建同卷 NTFS 校准对象并完成删除前、delete-pending、实际关闭后观察，再用真实正向目录控制核对 8／16 字节 FileID 查询。只有已登记 `Retiring` 的确切 FID 得到校准缺失结果时，才生成新的 opaque 退休证明。原路径缺失不能代替该证明；原路径出现不同 FID 的文件时，该文件保留。已 ACK 的角色不再次打开或删除，journal 本身已经删除的窗口也可处理。

Coordinator 初始化失败前已经登记的操作，在持久 `installation_attempted=Some(false)`、三个角色均明确从未分配且当前恢复许可有效时，可以恢复未启动义务并提交 unopened／unallocated ACK。普通尝试标记、旧文档未知状态或创建后尚未登记 FID 的状态继续保留额度。

原文件关闭前，`TagOriginalRetirementSeal` 通过仍持有的实际旧 writer 封存最终文件观察及已提交事实摘要，Repository 持久登记 `original_retirement_intent`。旧句柄实际关闭后提交的回执仍为 `original_close`。

关闭完成但该 ACK 尚未提交时，具有匹配持久 intent 的 `Published`／`Restored` 操作可生成独立的 `TagOriginalRetirementRecoveryWitness`。新的同对象 RW 控制句柄使用零共享；其存活期间，实际只读零共享准入必须拒绝；明确关闭该控制后，只读零共享准入必须成功；该只读句柄存活期间，实际 RW 零共享准入必须拒绝。每个新句柄均核对实际路径、FID、单链接、NTFS 卷、长度和纳秒修改时间，完整 body SHA 必须匹配原封存事实。所有控制句柄均不写文件内容或更新时间，不在音乐目录创建探针文件。

明确关闭新的零共享只读句柄后，取得同 FID 的 `READ_DATA`／`shareREAD` 稳定读取 lease，禁止新的数据 writer、DELETE 准入和重命名。在该 lease 下重新验证完整 SHA、路径、FID、卷、长度及纳秒时间戳，覆盖两个读取句柄之间的准入间隙；lease 持续到 SQL ACK。`original_recovery` 单独记录该准入证明，`original_close` 保持为空；ACK 成功后明确关闭 lease 及新命名空间句柄，再处理工作文件退休。失败保持实际句柄和额度，重试使用同一证明。

缺少持久关闭 intent 的历史操作、实际排他准入 Busy、文件身份或内容改变、校准语义未知时继续保留额度。新的句柄关闭不能冒充旧 `CloseHandle` 结果；该恢复路径不刷新 Repository 的原文件 SHA 或时间戳。

## 验证范围

`repository_tag_edit` 已验证的 23 项测试覆盖独立写权、撤销与绑定 CAS、真实安装事务与列表／outbox、一致回滚和重启账本、实际关闭与最终时间戳、删除后失败 ACK 的 witness 重试、跨操作 4 GiB 预留拒绝及 schema 15 到 16 的真实迁移备份。

`library_tag_install` 的 13 项普通测试通过，独立子进程实际执行八个持久阶段的退出与恢复。另有 5 项真实 native 退休测试及 3 项 Backend 编辑闭环测试通过。原生测试包括实际 READ_DATA 稳定 lease 下拒绝 WRITE_DATA、DELETE 与重命名，以及新控制句柄实际关闭失败时保留原描述符。Backend 验证实际 Playing／Paused 占用时拒绝新写入、Stop 后编辑、成功完成意图在播放中的准确重放，以及实际 OS 读取者导致失败后拒绝将退休完成重放为编辑成功。音频 generation 与内容保持一致。所有文件测试使用隔离临时目录与自有编码音频。

`repository_tag_edit` 的冷恢复测试目标覆盖三个角色实际删除与关闭后、SQL ACK 前退出并重启，以及 Coordinator 的实际 SQL 初始化失败、同名不同 FID 文件保留、旧尝试字段缺失和原关闭 ACK 缺失时继续保留额度。

原退休恢复测试目标另覆盖实际关闭后、SQL ACK 前退出，持久关闭 intent 与独立恢复回执、恢复 ACK 故障时实际 lease 保留、同对象排他 writer 的真实 Busy、内容改变拒绝及旧 intent 缺失。原生测试核对新控制句柄关闭失败时的实际描述符保留，以及只有稳定数据读取 lease 存活时 WRITE_DATA／DELETE 准入及重命名均被拒绝。

真实 FLAC 的 700 KiB 引号歌词用例验证 JSON 膨胀在原写入前拒绝：原文件全部字节、纳秒修改时间和 binding revision 保持不变，实际所有者退休后额度归零，无提交或回滚回执。

全局文档压力测试完成 15 个真实 900 KiB 元数据安装及实际退休，再并发提交四个真实选择，验证未来文档预算在 native 文件创建前拒绝超额操作。旧账本接纳失败后，全部许可／操作／artifact 文档、revision、读取授权及原字节保持不变。冷恢复容量测试保留 63 份未使用 Install 许可，第 64 份执行真实安装并在退休 ACK 前退出；重启后独立 Recovery 批准成功，实际资源退休后恢复槽归档回收。
