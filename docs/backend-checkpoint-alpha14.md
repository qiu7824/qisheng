# LX Native CLI 0.3.0-alpha.14

Windows x64 后端 CLI 使用 Rust 1.99.0、SQLite schema 16 和锁定依赖，关闭 `ui` feature。[程序](../bin/lx-native-cli-0.3.0-alpha.14.exe)为 78,668,288 字节，[SHA-256](../bin/lx-native-cli-0.3.0-alpha.14.sha256)为 `926B2FD679875098DD4B5037A11C0773908F2478D55CC8346C7B17B2514F3E52`。交付路径与构建产物哈希一致，`--help` 正常退出并报告 alpha.14。桌面发行产物仍为 0.2.0。

## 功能范围

[标签 Backend/CLI 协议](backend-tag-edit-api.md)提供七个命令、四个查询、明确写许可、同操作编辑意图幂等、有界工作线程、实际 owner 恢复与取消。MP3/FLAC 标签保持原文件 FID 和编码音频 SHA-256；标签、资产绑定、列表副本和 outbox 在同一事务提交。原 writer 实际关闭及三个工作文件的退休 ACK 完成后才释放额度和标记 `finished`。

[物理恢复文件组](backend-physical-restore.md)提供原 Guard 下的 DB/WAL/SHM/rollback journal/JSON 安装、禁止覆盖的原生移动、实际 SQLite 主文件 FID 校验及进程内回滚。关闭时的 revision 与主文件身份约束安装入口；当前 JSON 字节保留。完整冷启动恢复、安装提交、终态清理与当前 SQL 授权保留仍需完成。

## 验证范围

全部无界面库、CLI 和测试目标的严格 Clippy 检查通过，使用 `-D warnings`。资源装箱后，连续锁交接、失败初始化、actor panic 与实际工作线程退休测试通过。

完整库回归为 **338 项通过、1 项失败、2 项默认跳过**。失败项为 `audio::supply::feed_tests::actual_cursor_and_catalog_feed_at_prepared_and_foreground_pcm_capacities`；独立复测保持原断言仍失败。DirectCursor 两种容量均无 underflow；CatalogRangeCache 的 80ms 容量在一秒输出窗口仅呈现约 1.436 秒媒体，目标为 2 秒，累计 underflow 12,674 帧。媒体时钟与实际消费帧一致；缓存供给性能尚未通过验收。

以下七个集成目标共 **60 项通过、0 项失败**。安装器崩溃 helper 由八个实际子进程执行，独立 harness 条目默认跳过；原生桌面媒体会话测试默认跳过。

| 目标 | 通过 |
| --- | ---: |
| `backend_tag_edit` | 1 |
| `library_tag_install` | 13 |
| `repository_tag_edit` | 11 |
| `cli` | 15 |
| `backend_profile_handover` | 5 |
| `backend_physical_snapshot` | 7 |
| `system_control` | 8 |

库测试包含真实 CloseHandle 保护失败、只读 disposition 失败和 held reader 阻止 FID 缺失确认；新 profile 的公共 Backend 标签测试没有预创建工作目录，并检查实际标签、音频 SHA、最终退休和关停后额度归零。

## 剩余范围

标签删除并关闭后、SQL ACK 前崩溃的冷恢复，以及 coordinator 建立前预留额度的恢复窗口保持 pending 和收费，尚未闭合。物理恢复 Backend/CLI 协调、完整文件组冷恢复与清理、缓存供给性能、完整音效偏好、下载覆盖与成品播放、官方同步及平台互通、GUI URI 激活、硬件与新机验收和完整发行回归仍需完成。M00–M11 尚未全部完成。
