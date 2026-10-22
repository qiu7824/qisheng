# LX Native 0.3.0-alpha.4 后端检查点

日期：2026-10-07。平台：Windows x64/MSVC，Rust/Cargo 1.99.0，SQLite schema 5，后端协议1。兼容基线为LX Music Desktop v2.12.6提交`ad95d5091c9ed689fa72b5e5c849df65f5a679ce`。

## 可运行范围

alpha.3的独立后端、五平台SDK、事务列表、持久队列与歌词链路保留。音频生产路径使用有限块缓存、限定HTTP/1、显式Symphonia解码、固定PCM队列和直接CPAL/Memory渲染，已移除整首64 MiB与总加载20秒限制。连接、响应头、期待数据期间的idle、重试、对象和内存/磁盘配额各自有界。

已批准源的预加载按照真实Ended队列预选候选及其实际质量，精确URL批准前不发媒体GET。同一engine持有唯一投机decoder/transport/PCM source，实际提升复用原资源；队列、源、批准、seek、缓存及表示变化使旧资源失效。异步nonce/generation诊断与真实输出确认避免把入队受理误当播放成功，失效时重新解析候选并等待新批准。

播放解析、投机解析与local歌词沿用一个JavaScript会话，前台请求优先。普通operation取消清理HTTP/timer/job并保留闭包；初始化取消、停用、删除、换源和关闭明确终结生命周期。受管缓存登记、OS owner锁、两槽manifest及ownership header支持死亡owner恢复，陌生内容与链接保留并报告cannotClear。

## 验证证据

| 检查 | 结果与范围 |
| --- | --- |
| 无界面统一回归 | 51个套件，478项通过，0项失败，1项物理设备测试跳过 |
| 大文件真实播放器 | 68 MiB原创WAV首PCM早于传输完成；中/尾段频率定位、暂停seek、EOF、结束后重新seek与关闭通过 |
| 长传输真实播放器 | strong206第二256 KiB块分84次持续21秒发送；首PCM、Buffering内容钟冻结、恢复后PCM/时钟、30秒尾段880 Hz与Stop/shutdown通过 |
| HTTP/缓存 | 27项传输、9项缓存、12项恢复；206/200/416、强弱validator、未知长度、短响应/重试、表示变化、活动清理及实际子进程死亡/OS锁/reparse/foreign保留 |
| 投机传输 | 8项实际HTTP；2 MiB累计DATA含失败重试与头尾seek，200与大chunk预算等待、超过idle期间等待、同lease提升、取消唤醒、末CL精确EOF和零请求拒绝 |
| 解码与输出 | 7项decoder、6项PCM、3项真实Memory output及9项渲染内核单位测试；seek/epoch/EOF、整包scratch全局配额、源交接及数据回调分配/销毁为零 |
| 预加载 | 23项领域、6项audio、6项backend；候选质量独立、精确批准前零GET、单GET原资源提升、暂停/静音、失效回退及实际JSON Lines CLI闭环 |
| 音源取消 | 6项实际worker操作取消及真实容量1通道拥堵单位测试；异步HTTP关闭、旧timer/回调隔离、闭包计数保留、取消初始化终态及歌词重载请求2确认 |
| HTTP/TLS客户端 | 6项真实HTTP/TLS；8 KiB DATA/协议边界、过大响应头拒绝、无redirect、默认拒绝自签证书及私有测试信任库正向TLS |
| 静态检查 | fmt、全部无界面目标严格Clippy、默认GPUI特性全部目标编译检查通过 |
| 正式路径CLI | 版本/能力、静态worker/明确源批准、实际流式Memory PCM、暂停准确seek、缓存清理终态、正常关闭、音量持久与选中批准源按需重启通过 |

统一Cargo总数不重复累加专项和烟测。网络与PCM使用项目原创fixture；这些结果不代表所有真实平台音频质量或硬件发声均已验收。

## 开发产物

文件：[lx-native-cli-0.3.0-alpha.4.exe](../bin/lx-native-cli-0.3.0-alpha.4.exe)。大小：39,879,168 bytes。

SHA256：`1051FEF0895BB5778A9C667C735A423E8C4EEEBBD74E0E19EF3342D753C2AF77`。

CLI默认Memory；`--audio device`选择真实输出。`preload`、`approve-preload OPERATION URL`、`cancel-preload OPERATION`、`cache-stats`、`clear-cache`及JSON协议见[README](../README.md)。旧alpha.1–alpha.3与0.2.0发行包保留原验证状态。

## 未完成边界

完整M05及M00–M11后端交付尚未完成。缓存发布、清理和最终lease/pin释放仍包含同步文件系统操作，大缓存控制p95 100 ms与关闭期限待验收；cache对象使用stream serial，跨播放/重启音频复用尚未实现。无脚本本地资产预加载、匹配及持久URL/图片缓存继续实施。

目录入库/roots索引、设备/DSP、下载与后处理、完整备份互通、LX v4同步、OpenAPI/系统控制及完整发布按[实施规划](backend-port-plan.md)推进。当前source会话的同步JS/native调用采用既有CPU/迭代与进程期限约束，协作取消不硬抢占同步代码。前端布局与视觉设计保持后续阶段。
