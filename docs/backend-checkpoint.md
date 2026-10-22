# LX Native 0.3.0-alpha.1 后端检查点

日期：2026-10-06。平台：Windows x64/MSVC，Rust/Cargo 1.99.0，数据库 schema 3，后端协议 1。

## 可运行范围

独立命令、查询与事件；JSON CLI；SQLite 事务、迁移、outbox 与 profile writer 锁；稳定本地路径身份、标签、封面、旁车与文件 Range 服务；受限源进程和四轨歌词 IPC；原文/编辑歌词、恢复、导出、实际音频时钟行/字进度；Memory PCM 与基础设备输出入口。现有 GPUI 界面通过适配层使用同一后端，视觉布局保持现有实现。

完整在线五平台 SDK、五模式队列、下载管理、备份互通、同步、OpenAPI、设备选择及 DSP 等范围仍按[后端规划](backend-port-plan.md)推进。目录索引与供应商歌词 codec 尚未完整接入；本地大文件可以探测和入库，但播放 URL loader 仍受 64 MiB/20 秒限制。

## 验证证据

| 检查 | 结果与范围 |
| --- | --- |
| 无界面库与全部集成测试 | 一次完整运行 169 项通过，1 项真实设备测试跳过 |
| 解析期间暂停/恢复回归 | 2 项实际 worker、HTTP、Memory PCM 测试通过；解析完成遵循最近播放意图 |
| 歌词重载回归 | 2 项实际 HTTP/worker 测试通过；借出 worker 取消后重新获取，缓存命中不执行 JS |
| 暂停后倒退定位 | 实际 8000 Hz PCM 连续定位测试及歌词帧闭环通过；解码器设置长度与随机访问 |
| 并行音频复现压力 | 10 轮并行测试通过；HTTP fixture 明确取消 Windows accept 继承的非阻塞状态 |
| 格式检查 | 当前检查点通过 |
| 严格 Clippy | 所有无界面目标通过，warnings 作为错误检查 |
| 默认 UI 特性编译 | 所有目标通过编译检查 |
| 开发 CLI | 正式路径的 `--help`、能力查询及真实独立进程静态源导入通过，版本为 0.3.0-alpha.1 |

统一回归覆盖 169 个通过用例。真实声卡输出、系统媒体集成、目录扫描、供应商 codec 和在线平台联测不由 Memory PCM 结果替代。

检查点统一验收状态：通过。完整后端状态：未完成。

## 开发产物

文件：[lx-native-cli-0.3.0-alpha.1.exe](../bin/lx-native-cli-0.3.0-alpha.1.exe)。大小：31,418,368 bytes。

SHA256：`FE5C85D3ED45CA95AC3B26AE50DE1D777576045D10211E3BEFDC4B3FC285FD93`。

CLI 默认 Memory 模式；`--audio device` 明确选择实际输出设备。数据目录、命令及事件恢复方式见 [README](../README.md) 和[后端开发状态](backend-status.md)。普通退出执行显式关闭，单次 Accepted 操作等待终态，JSON Lines 可在操作期间继续查询和取消。
