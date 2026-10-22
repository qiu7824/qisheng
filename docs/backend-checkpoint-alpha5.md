# LX Native 0.3.0-alpha.5 后端检查点

日期：2026-10-07。平台：Windows x64/MSVC，Rust/Cargo 1.99.0，SQLite schema 5，后端协议1。兼容基线为LX Music Desktop v2.12.6提交`ad95d5091c9ed689fa72b5e5c849df65f5a679ce`。

## 可运行范围

alpha.4的流式音频、五平台SDK、源会话、持久队列、歌词和实时渲染继续保留。受管缓存由独立有界I/O worker负责初始化、发布、登记、审核与回收；控制端的lease、pin、最后释放、清理epoch及RAM读取使用短锁，磁盘读取在解码线程使用不可变ownership proof和reader hazard。

物理文件删除与持久登记成功前保留退休存储占用，清理完成回执覆盖实际GC与审核。活动对象及清理后未来发布继承延期状态，取消、表示变化及关闭后的迟到发布不能重新激活失效块。原始资源、陌生文件、替换ownership header及链接目标保留；无法完成清理时明确报告失败。

Engine、decoder、cache与Tokio Runtime的thread handle在启动前预约有界owner槽。超时关闭保留可查询、再次等待的真实owner，最终join完成才返回成功；Drop只发取消或唤醒。Runtime始终由其owner线程正常销毁，初始化失败以TransportInitError承接该owner；stream未来体的RAII完成标志覆盖正常结束、未poll及中途abort。

本地资产预加载使用明确的CatalogAsset authority与资产内容stamp；源安装采用独立Source authority。已登记文件与原创catalog音频沿用资产批准，媒体身份的source SHA为空、source policy为0，避免虚构音源。候选资源直接移交同一播放器；资产重新导入后旧准备资源失效，播放使用新内容。

## 验证证据

| 检查 | 结果与范围 |
| --- | --- |
| 无界面统一回归 | 54个套件，505项通过，0项失败，1项物理设备测试跳过 |
| 实际锁阻塞 | 自进程持真实registry OS锁，以production io_waiting_for_registry确认进入等待；初始化、clear barrier超时如实返回，控制端不等待文件锁 |
| 控制响应 | 20样本constructor/final Drop、MediaSource Seek/RAM读/status/cancel、Backend快照/缓存/元数据/游标和Seek确认p95均小于100 ms；正常关闭及释放后actual join覆盖 |
| 生命周期 | bounded owner容量、TLS teardown、panic join、decoder阻塞read/超时重join/最后caller Drop、Runtime真实spawn_blocking、stalled-header和无效cache构造失败归属 |
| 缓存/传输 | 原9项cache、12项恢复、7项decode、6项PCM、6项audio preload、28项transport、8项投机预算继续通过，foreign/配额/头替换/reparse边界保留 |
| 本地资产 | 2项实际文件桥与Memory：无源、无新URL批准的原资源提升及880 Hz原PCM；重新导入后旧资源未提升、实际新660 Hz PCM；24项纯预加载验证authority与内容stamp |
| 静态检查 | fmt、全部headless目标严格Clippy、默认GPUI特性全部目标编译通过 |
| 正式路径CLI | 版本/能力、静态worker/源批准、真实流式PCM、暂停准确seek、缓存终态、关闭、音量持久及按需源重启通过 |

统一Cargo总数不重复累加专项与烟测。锁阻塞测试验证该工况的响应目标；全部文件系统故障、真实平台音频和硬件发声需要各自验收。Backend总冷启动包含SQLite迁移与原创音频构造，不属于100 ms控制响应指标；独立五组诊断无锁冷启动平均440.449 ms、持锁439.426 ms。

## 开发产物

文件：[lx-native-cli-0.3.0-alpha.5.exe](../bin/lx-native-cli-0.3.0-alpha.5.exe)。大小：40,148,480 bytes。

SHA256：`731F6D2A95DF8CD7EB4A4E7B0F88567CC3BD201052DDD058A44F71978D76C7AA`。

CLI默认Memory；`--audio device`选择真实输出。旧alpha.1–alpha.4及0.2.0产物保留各自证据。接口、配额与正式构建入口见[README](../README.md)。

## 未完成边界

完整M00–M11后端尚未完成。M05仍包含跨平台匹配、持久URL/图片缓存和跨播放音频缓存复用；目录入库/roots索引、设备/DSP、下载与后处理、完整备份、LX v4同步、OpenAPI/系统控制和完整发布按[实施规划](backend-port-plan.md)推进。真正不可取消的内核系统调用可能超过请求期限，其owner保持归属，返回结果不会虚构已关闭。前端设计保留后续阶段。
