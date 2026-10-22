# 物理 profile 安装与退休

Windows NTFS 的物理恢复内核使用实际文件句柄准备 SQLite 镜像，并通过同一 `ProfileGuard` 安装关闭后的 profile 文件组。它与[物理快照导出](backend-physical-snapshot.md)的 Backend 命令及恢复账本是独立接口。

## 准备与连续锁

`PhysicalRestoreSelection::open_selected(path)` 显式选定一个不可变的独立 SQLite 镜像。镜像必须没有 WAL、SHM 或 rollback journal，大小不得超过请求额度，schema 必须位于受支持范围。

`Repository::prepare_physical_restore(request, selection, context, limits, register)` 复制、验证和清理私有候选，保留实际源文件、临时文件、manifest 和收费额度。`PhysicalRestoreRequest` 包含 `operation_id`、`expected_revision`、`maximum_image_bytes`、`selection_epoch`；`register` 接收只用于定位的 `PhysicalRestoreLocator`，该 DTO 不提供重新打开、写入或删除权限。

准备不替换当前主库或 JSON。候选经过 schema、integrity、foreign-key、迁移、权限清除、当前授权覆盖、VACUUM 和完整 SHA-256 校验。导入图像中的 pairing、文件／目录读取许可、下载执行许可和标签写许可不会成为当前 profile 的执行能力。当前授权在同一 SQL 读取事务中逐行复制到已拥有的候选库，保存当前曲目、资产、定位、目录、监视设置及已完成操作记录，并生成同一切面的摘要；同 ID 的导入文档由当前实际授权对象覆盖。输入累计上限为 1 GiB、1000000 行，单行上限为 8 MiB 加 64 KiB，同时最多 4 项操作；这些输入计数不增加实际图像或 sidecar 的既有配额，也不在内存中保留整份授权表。存在未退休工作时拒绝物理替换。

安装必须使用当前 Repository 真正关闭后返回的同一个 `ProfileGuard`。关闭过程取得实际 SQLite 独占锁，完成 WAL 截断检查点，在同一连接中捕获 revision 和授权摘要，并保留实际主文件证明跨越连接关闭。只有 SQLite 真正关闭成功后才确认关闭状态；随后核对完整内容及关闭后的变化戳。原生证明句柄关闭失败时，同一 guard 保留原句柄，通过 `finish_close()` 重试。

`PreparedPhysicalRestore::install_files(guard, context)` 验证连续锁、实际 root、关闭 revision、授权摘要及实际主文件完整内容和变化戳。关闭后的同文件身份内容修改与新出现的 sidecar 均导致拒绝。重新申请同名路径的锁不能代替这个进程内连续锁证明。`PhysicalRestoreImageView.authority` 仅报告覆盖结果，不能重建授权。

Backend 的 profile quiesce 应先完成 actor、音频、library、picture、下载、标签、系统控制、relay 与 Catalog 的实际停止及 join，再移交同一 guard。超时或停止失败保留实际所有者，不能提前替换文件组。

## 实际文件组

文件组包含 SQLite main、存在的 WAL／SHM／rollback journal，以及存在的 `player-state.json`。安装器通过目录 pin 与相对 no-follow 操作取得实际文件身份、字节数和 SHA-256；不使用路径字符串代替文件所有权。

当前 JSON 原字节复制到 replacement，保留当前 profile 的 Source 选择、明确批准与系统设置。导入 SQLite 中的权限先清理，再覆盖当前 SQL 授权；备份内的路径、身份声明及批准字段不能扩大当前访问范围。

当前已批准同步设备的私有凭据和模式来自当前授权快照。恢复后的数据集将当前 `profile_epoch` 与 `sequence` 各推进一次，并重置基线、历史快照及增量日志；旧会话、游标和确认失效。已批准设备重新认证并完成新的全量同步后，才能继续增量同步。

每个实际创建前先写 allocation intent，再确认同一实际空对象的 FID。rename 使用已持有的同一文件句柄；原文件保留为私有 old 对象，replacement 保留为 new 对象。目标存在 foreign 文件时安装或回滚失败，不覆盖、不删除该 occupant。

独立 group journal 位于 `.lx-snapshot-staging`，记录 request、候选图像证明、prepare manifest／main 的实际身份、原文件与 replacement 的 FID／SHA、allocation intents、当前位置和清理事实。每条记录含长度及 SHA-256，最多 64 条、单条 64 KiB、总量 1 MiB。已确认前缀不改写；同一所有者可以重试其未确认的 append 尾部。

## 打开、回滚与终态

| 接口 | 实际保证 |
| --- | --- |
| `installation.reopen(context)` | 第一个 SQLite writer 必须打开已记录的实际 main；不创建缺失主库、不初始化替换路径。失败保留打开过程或 Repository 所有者。 |
| `installation.rollback(context)` | 先实际关闭新 Repository，再把同一 replacement 移回私有位置、将同一 original 恢复到 live 位置。foreign occupant 保留。 |
| `installation.commit(context)` | 要求新 Repository 已成功打开；退休所有 old 对象、group journal、prepare 临时对象和 manifest，完成后返回 terminal。 |
| `installation.finish_rollback(context)` | 要求原 Repository 已成功重新打开；退休 replacement、group journal、prepare 临时对象和 manifest，完成后返回 terminal。 |

终态方向保存在 journal 中，重试不能把 commit 改为 rollback cleanup。`PhysicalProfileInstallFailure.owner` 是同一个 `PhysicalProfileInstallation` 的实际所有者；错误不会以新路径、新句柄或无效果占位对象替换它。

`PhysicalProfileTerminal` 提供 `operation_id()`、`installed()` 与 `into_repository()`。成功移交的 Repository 继续持有同一实际 profile 锁。调用方可以继续交给 Backend 启动；需要再次关闭时，通过 Repository 的真实 close 返回 guard。

## 退休与额度

每项操作最多准备 1 GiB 图像，工作空间预留为 `3 × maximum_image_bytes + 65 MiB`；所有操作共享额度上限 4 GiB，最多 4 个未完成操作。原文件组、replacement、prepare 图像与 manifest 使用同一项预留。

文件退休复用 `snapshot_native::ActualFile` 的 NTFS 校准引擎。它从同一实际 FID、长度、完整 SHA 和单链接证明开始，持久记录 delete disposition，真实关闭句柄，并以已校准的 exact-FID 查询确认物理移除。路径缺失不提供删除或退款证明。

原始 `CloseHandle` 或校准句柄关闭失败时，原 raw handle 重新放回同一 `File` 所有者；重试真正执行关闭。确认后的物理清理事实还须完成 journal append。append 失败保留实际退休所有者，不能仅依据内存中的 removed 标志释放额度。

删除 group journal 之前，prepare manifest 保存完整最终 group record；group journal 的实际 disposition／close 事实写入仍存活的 prepare manifest。所有已知私有对象与两个 journal 实际退休之后才释放操作预留。

## 冷恢复范围

进程内安装、重新打开、回滚、两个终态清理接口及当前 SQL 授权覆盖已有实际验证。冷恢复内核覆盖真实子进程在 rename 与终态清理窗口退出后的恢复，并拒绝伪造的父创建目标和文件身份。完整 Backend／CLI 重启入口、默认 WAL／SHM 创建所有权及 50k 曲库恢复容量仍待验收。

冷恢复必须要求新的明确实际选择、当前 profile guard、native directory pins 和实际 FID／完整 SHA；journal 与 locator 只描述旧操作，不能构造写能力。外来文件、未记录实际 FID 的 allocation、缺少物理退休证明的对象保留失败状态与收费，不能通过名称不存在清零。

## 验证范围

`repository::profile_install` 与 `repository::profile_guard` 的实际定向测试覆盖独立图像、净化及当前授权覆盖、额度与失败保留、实际文件组安装／打开／回滚、foreign main／sidecar 保留，以及 commit 和 rollback 的终态清理、双 journal 删除、额度归零与连续锁。当前授权用例包含同 ID 伪定位拒绝、无 revision 的撤权变化、关闭后同文件身份内容修改及当前设备凭据重新认证和全量／增量同步；同步网络用例运行两个本程序后端，不作为官方进程互通证据。

`repository::snapshot_native` 的 7 项实际文件测试通过，包含 `HANDLE_FLAG_PROTECT_FROM_CLOSE` 下原 `ActualFile` 句柄保留、解除保护后重试真实关闭。实际后端交接与安装回滚还验证了 SQLite 关闭后证明句柄受保护时保持原 guard、拒绝提前交接或移动文件，解除原句柄保护后继续完成。文件测试使用独立临时 profile 和自有数据，完整验收范围见[后端开发状态](backend-status.md)。
