# LX Native CLI 0.3.0-alpha.13

Windows x64 后端 CLI 使用 Rust 1.99.0、SQLite schema 16 和锁定依赖，关闭 `ui` feature。[程序](../bin/lx-native-cli-0.3.0-alpha.13.exe)大小为 77,318,656 字节，完整性记录见[SHA-256](../bin/lx-native-cli-0.3.0-alpha.13.sha256)。交付路径的哈希与构建产物一致，`--help` 正常退出并报告 alpha.13。

[连续 profile 交接](backend-profile-handover.md)覆盖 actor panic、观察方退出、真实工作线程阻塞、初始化失败、SQLite close 和原 guard 重启。工作线程 panic 保留原错误，实际关闭后可显式取回失败 guard；待冷恢复的持久账本保持原状态，已经安全释放的句柄不再阻止实际 Runtime 退出。

[物理候选库](backend-physical-restore.md)提供不可写源句柄、64 KiB 复制、第一条 SQL 前的解析与取消限制、历史生产 DDL 校验、schema 迁移、权限清除、压缩和封存。它保留失败实际拥有者和额度，尚未执行数据库组安装。

[本地标签仓库](backend-tag-edit.md)提供独立写批准、撤销、编辑意图幂等、同独占句柄证据、真实 Repository CAS，以及标签、绑定、列表副本和 outbox 的原子提交。MP3/FLAC 原文件、恢复文件和各持久阶段的进程崩溃由实际文件测试验证。

## 验证范围

所有无界面库、CLI 和测试目标的编译检查通过。库回归为 324 项通过、0 项失败、2 项显式原生测试默认跳过；受影响的 6 项生命周期测试复测通过。

以下 18 个集成目标共 160 项通过。实际崩溃 worker 作为子进程执行，独立 test harness 条目默认跳过；原生桌面媒体会话测试仍默认跳过。

| 目标 | 通过 |
| --- | ---: |
| `backend_instance_relay` | 3 |
| `backend_profile_handover` | 5 |
| `cli` | 14 |
| `library_tag_install` | 9 |
| `library_tag_write` | 4 |
| `repository_tag_edit` | 5 |
| `system_control` | 8 |
| `repository` | 18 |
| `repository_artwork` | 10 |
| `repository_library_binding` | 13 |
| `repository_library_roots` | 19 |
| `repository_lists` | 9 |
| `repository_media` | 10 |
| `repository_native_backup` | 11 |
| `repository_physical_snapshot_ledger` | 6 |
| `repository_playback` | 7 |
| `repository_sync_migration` | 2 |
| `backend_physical_snapshot` | 7 |

## 剩余范围

DB/WAL/SHM/JSON 成组物理安装、冷启动安装恢复、失败回滚和物理恢复 Backend/CLI 入口尚未完成。标签原写句柄和 journal/backup/candidate 的实际关闭与退休确认、额度释放以及 Backend/CLI 编辑命令尚未接线。完整音效偏好、下载覆盖与成品播放、官方同步及平台互通、GUI URI 激活、硬件与新机验收、完整 Clippy/GPUI 编译和发行回归仍待执行；M00–M11 尚未全部完成。
