# LX Native 0.3.0-alpha.11 后端检查点

日期：2026-10-08。Windows x64/MSVC，Rust/Cargo 1.99.0，SQLite schema 8，后端协议1。兼容基线为 LX Music Desktop v2.12.6 提交 `ad95d5091c9ed689fa72b5e5c849df65f5a679ce`。

## 可运行范围

本地曲库提供显式目录授权、后台有界扫描、分批事务入库、取消、目录健康和资产分页。显式文件授权与目录成员分别保存；旧资产在schema迁移中一次保留原文件授权，目录扫描不向JSON文件批准数组写入目录成员。撤销或收窄目录推进授权epoch，停止相关旧读取、预加载和音频；其他有效目录或显式文件授权继续生效，用户曲目、列表和歌词保留。

目录范围由实际祖先句柄与物理身份证明，Unicode文本lower比较不作为授权依据。源、元数据、封面、歌词及精确同名旁车通过已验证句柄读取，每个clone有独立位置，每64KiB复核撤销；probe累计16MiB且错误不能假报EOF。Lofty标签失败时既有可用标签继续保存，扫描seen与元数据成功分别登记，不把无法读取或未访问项误判为缺失。

两条已登记文件worker处理8项请求队列，独立2项completion队列采用单批ACK和真实文件guard。每批最多200项/1MiB，事务在逐项与实际commit前复核取消和root token；只有完整发现范围才执行缺失finalizer。健康Unknown、Verified、Unavailable和NeedsReapproval由后台物理验证及纯查询表示，成功proof在真实存储和句柄回收完成前保持预算占用。profile关停等待真实worker join及本profile实际句柄退休完成；进程期退休supervisor单独管理并保持owner。

完整接口、JSON和限额见[本地曲库接口](local-library.md)。

## 验证证据

| 检查 | 结果与范围 |
| --- | --- |
| 无界面统一回归 | 77个套件770项通过、0项失败、2项物理设备测试跳过 |
| 曲库闭环 | 原创401/1600文件、多批SQLite提交、资产keyset与revision、目录Add不隐式扫描、取消和部分扫描不误missing、撤销写JSON并重启无授权扩大 |
| 读取权限 | 实际HTTP/PCM/LRC/嵌入图片、递归收窄、多个目录与显式文件OR、根目录替换、真实Unicode双目录和跨batch身份冲突、同句柄独立游标 |
| 数值与边界 | 原音频/EQ/倍率回归，真实WAV probe预算、损坏标签保留、同内容重扫及None→Some证明加强保持原播放/generation/许可/缓存 |
| 真实资源回收 | ACK前后文件guard、保留inactive factory、后台验证proof、取消、异常worker及所有worker实际join后才报告失败、关闭超时重试与profile writer重开 |
| 静态检查 | fmt、严格无界面Clippy、默认GPUI全部目标编译通过 |
| 正式CLI | 20项烟测，包括既有流式/缓存/设备/音效流程及目录授权、扫描、真实PCM、撤销和重启 |

专项结果不重复计入统一Cargo总数。两项物理输出测试默认忽略，软件与受控端点结果不表示扬声器、驱动、CPU负载或声学延迟验收。Windows运行证据与Linux源码实现分别记录；其他平台安全目录打开未验收。50000项纯仓分页和预算不表示50000项运行时恢复、驻留峰值或控制p95已经通过。

## 开发产物

[lx-native-cli-0.3.0-alpha.11.exe](../bin/lx-native-cli-0.3.0-alpha.11.exe)，56,715,776 bytes。

SHA256：`19CDFF28D94EC73DC3133235E17165825EE040A00BC10B90CFE864222DD6AA67`。

[alpha.10检查点](backend-checkpoint-alpha10.md)与更早产物保留各自验证范围。CLI默认Memory，桌面入口沿用现有布局。

## 未完成边界

完整M00–M11继续实施。M02原生监听、文件重定位和标签写入，以及运行时大库预算与性能仍需完成。M05本地匹配及其余异常/真实平台分支，M06其余音效和系统媒体/快捷键/定时停止/电源与硬件验收，下载、完整设置/LX备份、v4同步、OpenAPI/SSE/深链、完整依赖许可与发布验收按[移植规划](backend-port-plan.md)推进。前端UI设计安排在后端验收之后。
