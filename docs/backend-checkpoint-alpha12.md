# LX Native CLI 0.3.0-alpha.12

数据库 schema 为 15。Windows x64 CLI 使用 Rust 1.99.0 构建，关闭 `ui` feature，使用真实 Memory PCM 或显式选择的物理输出。程序为[后端 CLI](../bin/lx-native-cli-0.3.0-alpha.12.exe)，大小 77,079,552 字节；完整性记录见[SHA-256](../bin/lx-native-cli-0.3.0-alpha.12.sha256)。

## 行为范围

有界 Range 预取保持原媒体许可、字节预算和 PCM 容量，覆盖倍速及原预加载资源提升。暂时的预取故障可延后到实际读取需求，权限与永久 HTTP 拒绝立即保留状态码，并阻止排队 PCM 继续呈现。当前完整 200 响应的受保护缓存可反向读取；弱验证及无验证响应仍不能跨流复用。

[系统控制](backend-system-control.md)提供持久设置、配置确认与回滚、排队输入取消和资源退休。[同用户单实例转发](backend-instance-relay.md)显式启用，第二 CLI 向持有实际 profile 写锁的实例转发 DeepLink；源与 URL 批准规则继续生效。SQLite 打开失败保留可检查、可取回的实际拥有者。

## 验证

所有无界面库、CLI 与测试目标的类型检查通过。以下集成目标使用锁定离线依赖串行运行，全部通过：

| 目标 | 通过 |
| --- | ---: |
| `audio_cache_latency` | 10 |
| `audio_player_long_stream` | 1 |
| `audio_transport` | 41 |
| `audio_transport_reuse` | 12 |
| `backend_instance_relay` | 3 |
| `backend_playback_processing` | 6 |
| `cli` | 14 |
| `system_control` | 8 |
| 合计 | 95 |

系统控制、配置回滚、DeepLink 帧及监督线程的 25 项库测试通过；启动退休的 3 项测试、Catalog 实际关机的 2 项测试通过。独立同用户管道测试显式执行并通过；实际手动重定位的同步归档身份测试通过。交付路径的程序哈希与构建产物一致，`--help` 正常退出并报告 alpha.12。

原生桌面媒体会话测试保持跳过。系统热键、电源通知、协议注册、物理输出、完整 GPUI 构建、新机和升级验收未由这些结果替代；全范围 Clippy 与完整发行回归仍待执行。

## 剩余交付

完整 actor panic 资源交接与连续 profile 锁、物理数据库安装、GUI URI 激活、本地标签编辑事务、下载覆盖与成品播放、完整音效偏好、官方 LX 同步及五平台服务互通、压力与实机验收继续列入完整后端范围。该检查点不表示 M00–M11 全部完成。
