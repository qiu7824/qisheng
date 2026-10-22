# 验证记录

最终验证为 **0.2.0：82 项常规测试全部 PASS，0 failed，fmt/check/build PASS**。原生 Windows DirectX 截图验收另行执行，1 项 PASS，产出三张 1536 × 1095 PNG；最终 EXE 打包及 `--help` 检查通过。实际 EXE 诊断的音源、HTTP POST、工具函数、取消和重启步骤均 PASS，但整体为 `BLOCKED`、退出码 2：当前执行会话的默认音频输出端点不可用。硬件播放验收独立受阻；完整证据见本文末尾 0.2 记录。

## 0.1 初版验证与原始证据

以下内容保留 0.1 的验证结果、诊断原始来源、截图与可执行文件哈希。54 项是初版测试总数；对应二进制现保留为 `bin/lx-gpui-player-v0.1.exe`。其中标记为未运行的项目，应结合文末 0.2 扩展覆盖范围判断，不能直接解释为新版测试状态。

验证日期：2026-10-04。初版结果为 **54 项常规测试通过，真实 Windows DirectX 界面截图通过，硬件音频验收受阻**。当前执行会话无法取得默认音频输出端点，错误为 `Could not find any output device`；这不能据此判断整台电脑没有声卡。

本记录将测试通过、测试失败和未运行分别列出。初版 [docs/diagnostics-v0.1.json](diagnostics-v0.1.json) 的整体结果是 `BLOCKED`，不能把其中已通过的音源步骤解释为完整硬件播放链路通过。

### 环境与交付位置

| 项目 | 本次记录 |
| --- | --- |
| 平台 | Windows x64，MSVC 工具链 |
| Rust / Cargo / rustup | 1.99.0 / 1.99.0 / 1.29.1 |
| 界面 | GPUI Kit 0.7.0；原生 Windows DirectX 渲染 |
| 固定参考 | LX Music Desktop v2.12.6，commit `ad95d5091c9ed689fa72b5e5c849df65f5a679ce` |
| 当前构建目录 | `C:\Users\xs\AppData\Local\Temp\lx-gpui-build-20261004\target`，见 [Cargo 配置](../.cargo/config.toml) |
| 操作脚本、日志及 Cargo 缓存根 | `D:\codex操作目录\lx-gpui-build-20261004` |
| 初版可执行文件 | [bin/lx-gpui-player-v0.1.exe](../bin/lx-gpui-player-v0.1.exe)，72,249,856 bytes；已运行 `--help` 和纵向诊断 |

构建目录因 D 盘空间不足迁移到 C 盘临时目录；源码与用户数据位置没有随构建缓存移动。Windows 可执行文件使用 16 MiB 主线程栈，未知音源的语法解析及执行均位于受限子进程内。

### 已执行的检查

以下命令在工程根目录执行，构建和测试使用锁定依赖：

```powershell
cargo fmt --all -- --check
cargo check --locked
cargo build --locked
cargo test --locked
```

| 检查 | 结果 | 说明 |
| --- | --- | --- |
| 格式检查 | PASS | `cargo fmt --all -- --check` |
| 编译检查 | PASS | `cargo check --locked` |
| 可执行文件构建 | PASS | `cargo build --locked` |
| 常规单元测试 | PASS，40 项 | 音源契约、曲库、存储、工作进程边界及真实内存 PCM 音频引擎检查 |
| 常规 UI 测试 | PASS，6 项 | GPUI 输入事件和命令分发，以及官方 headless 渲染支持查询 |
| 真实二进制工作进程 IPC 测试 | PASS，5 项 | 使用编译后的播放器 `--source-worker` 入口 |
| 实际控制器集成测试 | PASS，3 项 | 未批准导入、批准并初始化、停用/重启用、删除、A→B 切换与重启恢复；真实 worker 与存储 |
| 常规合计 | PASS，54 项 | 最终 `cargo test --locked`；不含下面单独执行的 ignored 验收测试；全部 0 failed |
| 原生界面截图验收 | PASS，单独执行 | 真实 Windows 窗口与 DirectX 帧读取，产出三张 PNG |
| 硬件音频验收 | FAILED，单独执行 | 默认音频输出端点不可用；错误为 `Could not find any output device` |

单独执行的验收命令：

```powershell
cargo test --locked --test ui capture_real_gpui_library_png -- --ignored --nocapture
cargo test --locked audio::tests::real_audio_device_playback_diagnostic -- --ignored --nocapture
```

### 音源、持久化与播放诊断

[初版诊断原始结果](diagnostics-v0.1.json) 使用本工程原创合法音源与合成音频，未运行未知第三方音源，也未读取其他播放器的私人配置。诊断要求独立的空数据目录，避免覆盖已有数据。

| 项目 | 状态 | 实际证据与边界 |
| --- | --- | --- |
| 静态导入 | PASS | 在临时隔离进程中校验；含 `throw` 的合法脚本不执行，导入后仍未批准 |
| 损坏脚本 | PASS | 非法语法被拒绝；错误不会把源标记为初始化成功 |
| 实际子进程初始化 | PASS | 本工程可执行文件的独立 worker 成功初始化，Windows Job 附加成功 |
| LX HTTP callback | PASS | 真实回环 HTTP GET `/catalog.json`；验证 callback 与展平 legacy `musicInfo` |
| 取消 | PASS | 取消幂等；已终止的 worker 不能继续解析 URL |
| 重启恢复 | PASS | 脚本、精确哈希对应的审批、选择和音量恢复；新 worker 能重新初始化及解析 |
| 存储完整性 | PASS，单元测试 | 修改脚本撤销批准并清除选择；损坏 JSON 不覆盖；版本/大小限制、备份替换与中断恢复 |
| 脚本闭包生命周期 | PASS，实际 IPC 测试 | 同一 worker 多次请求保留计数；重启 worker 重置状态；重复初始化被拒绝 |
| 控制器音源生命周期 | PASS，实际集成测试 | 不注入 snapshot；导入后仍未批准，批准后等待真实 local manifest；停用/再启用、删除当前源清除存储选择；两个原创源顺序切换仅启用新源 |
| 控制器重启恢复 | PASS，实际集成测试 | Drop 所有公共句柄并等待旧控制器退出，再启动；已批准脚本及选择恢复，重新初始化新 worker |
| 非 Promise handler / 超大脚本 | PASS，实际 IPC 测试 | 返回明确错误，不能虚报成功 |
| 真实默认设备播放 | FAILED | 在加载第一首合成音频时即出现输出端点错误；播放时钟未启动 |
| 硬件播放 / 暂停 / 定位 / 切歌 | NOT RUN | 硬件输出失败后，没有完成这些动作的实际播放验证 |
| HTTP 404 后硬件播放恢复 | NOT RUN | 未运行实际音频错误恢复链路 |
| 512 MiB 耗尽与 5 秒进程强制终止压力验收 | NOT RUN | 限制已实现；Job 附加、IPC 大小边界和运行时循环/任务预算测试不能替代压力验收 |
| 控制器并发切换源后丢弃旧请求 | NOT RUN | 代码使用取消和 generation 判断；本轮没有专门完成并发切换/删除的端到端验收 |

两项内存输出测试复用生产 `run_engine`、真实 HTTP 下载、rodio Mixer/Player 和 WAV decoder，消费真实 PCM（检查非零 RMS/peak，以及静音后的零值）；不以模拟播放时钟代替解码。`production_engine_with_memory_output_controls_real_decoded_pcm` 已验证未自动播放、播放、暂停时进度保持、暂停定位、音量/静音、切歌、toggle、404/坏音频后重新播放、停止；`production_engine_with_memory_output_finishes_replays_and_seeks_after_end` 已验证播放结束、重播、超大定位钳制和结束后定位恢复。二者均 PASS，但输出去向为内存，不能证明扬声器发声。

首次补测暴露了 Windows accepted socket 继承监听器非阻塞模式的实际问题：早到连接或满发送缓冲使读写返回 `WouldBlock`，连接随后中断，表现为 10053。服务已显式恢复 accepted socket 的 blocking 模式，并在 8 KiB/2 秒限额内读完完整请求头后解析。回归 `loopback_server_waits_for_delayed_fragmented_headers_and_sends_complete_wav` 验证延迟/分段请求头、HTTP 200、Content-Length、全部 1,984,544 bytes 波形和 45 秒解码，PASS。修复后完整常规测试通过，最终打包 EXE 的音源/HTTP/重启诊断也通过对应阶段。

初版 EXE SHA256：`E828B487F3CA94B73212D63A5722A1BD6564EF8D8A3B4C801D2BE6D838CD778D`。初版打包 EXE 的诊断进程退出码为 2，整体结果为 `BLOCKED`，与已记录的输出端点失败一致。

### UI 覆盖范围与真实截图

[常规 UI 测试](../tests/ui.rs) 使用 GPUI 实际 hit testing、文本输入与 Slider 事件，确认导航、搜索、导入入口、批准、启用、停用、删除、播放控制、队列、进度、音量及外部播放 URL 确认产生正确命令；还覆盖空态、加载、错误反馈和禁用按钮。源批准与启用是分开的动作，初次渲染未批准源不会自动派发执行命令。

这些常规 UI 测试用命令接收器替代完整控制器，并由测试更新 snapshot，因此证明的是用户输入与界面命令分发。它们不证明文件选择对话框、审批持久化、源删除后的后台清理或硬件播放全链路均已通过。

[实际控制器集成测试](../tests/controller.rs) 另外验证了源批准/选择持久化、顺序停用/删除/切换与完整控制器重启。这些测试使用真实后台线程、真实受限子进程和临时独立存储，没有模拟 snapshot；未触发硬件播放或原生文件选择对话框，也未制造在途解析请求切换的竞态。

单独的原生截图测试连接真实 `ControllerHandle`，使用独立临时数据目录，导入原创示例源并切换曲库、音源和队列页面，然后读取真实 DirectX 渲染帧。该测试通过控制器命令进行页面导航与示例导入；它与上述点击测试互补，仍不等同于完整的用户文件导入到设备发声验收。

三张图片的 PNG 头尺寸均为 **1536 × 1095**：

| 页面 | 实际截图 |
| --- | --- |
| 曲库 | [library.png](screenshots/library.png) |
| 音源 | [sources.png](screenshots/sources.png) |
| 队列 | [queue.png](screenshots/queue.png) |

三张截图已成功保存到 ChatGPT Library，名称为 `library.png`、`sources.png` 和 `queue.png`。Library 返回逐项成功；本机 Windows 不支持官方助手使用的 POSIX xattrs，故未宣称已写入该类本地属性，对应身份和版本保留在本任务操作记录。

### 复验与尚未完成的验收

在可以访问默认音频输出端点的会话中，重跑硬件 ignored 测试，并对独立空数据目录执行诊断：

```powershell
.\bin\lx-gpui-player-v0.1.exe --diagnose --data-dir PATH_TO_NEW_EMPTY_DIRECTORY
```

需要补齐实际播放时钟、暂停、恢复、定位、切歌、404 后恢复，以及完整控制器的源切换/删除并发场景。当前会话的输出端点问题不会被编译、界面截图或内存音频测试消除。

本项目仅实现 README 所列的 LX 音源子集；这些验证结果不表示任意 LX 自定义源均可兼容。具体 API 范围、上游归属及许可约束见 [README](../README.md) 与 [固定契约记录](upstream-contract.md)。

## 0.2 权限与网络能力扩展验收

验证日期：2026-10-04。已执行新版完整 `cargo test --locked`，**82 项常规测试全部 PASS，0 failed**。本次扩展没有访问实际公网第三方音源；HTTP 使用回环 mock，TLS 测试使用本项目生成的证书与测试场景。测试仅使用原创合法 fixture、合成音频和独立临时数据目录。

`cargo fmt --all -- --check`、`cargo check --locked` 和 `cargo build --locked` 均 PASS。原生截图 ignored 测试单独执行，1 项 PASS；不计入以下 82 项常规测试。

| 测试组 | 结果 | 实际覆盖 |
| --- | --- | --- |
| 单元测试 | PASS，62 项 | 现有源契约、存储与 PCM 音频回归，新增 HTTP、TLS、工具函数与 Catalog POST 覆盖 |
| 实际控制器集成 | PASS，5 项 | 真正的控制器后台线程、子进程、状态与存储；不注入模拟 snapshot |
| JavaScript 工具集成 | PASS，4 项 | Buffer 编码和共享/复制视图、常见方法和整数操作、真实密码学向量/随机字节、zlib 与大小边界 |
| GPUI 常规交互 | PASS，6 项 | 真实 GPUI 输入与命令派发；命令接收端和展示 snapshot 仍由测试提供，边界与初版相同 |
| 实际二进制 worker IPC | PASS，5 项 | 同工程真实 EXE 的隔离校验、执行会话与 IPC 边界回归 |
| 合计 | PASS，82 项 | 完整常规测试；不包括单独执行的 ignored 原生截图或硬件音频验收 |

### 批准迁移与生命周期

当前网络权限政策版本为 **2**，批准同时校验脚本 SHA-256。旧版本批准记录，以及缺少权限版本字段的批准记录，加载后会撤销批准并清除选择；再次显式批准前不会执行脚本或启动网络请求。当前版本批准在重启后保留，修改脚本或哈希仍使批准失效。

| 场景 | 结果 | 证据与范围 |
| --- | --- | --- |
| 导入、批准、停用、重新启用、删除 | PASS | 原创 fixture 经静态隔离导入后保持未批准；批准后等待真实运行时 manifest，再验证状态与磁盘存储 |
| 当前批准与选中源重启恢复 | PASS | 释放控制器和公共句柄并确认线程退出，再启动新控制器与新 worker |
| A → B 源切换 | PASS | 两个不同哈希的合法源先后显式批准；仅 B 选中并启用，A 保持已批准但停用 |
| 旧政策批准迁移 | PASS | 旧批准与选择被撤销；包含 `throw` 的合法脚本未自动执行，运行时未启动 |
| 初始化中停用、删除、关闭、切换 | PASS | 实际初始化 HTTP 请求连接保持未响应时触发操作；每种操作均在 2 秒以内关闭连接，旧初始化结果未覆盖新状态 |
| 修改脚本或哈希 | PASS | 存储回归验证批准撤销与选择清除，不能通过修改内容沿用旧批准 |
| 获取音频 URL 期间与实际播放并发切换 | NOT RUN | 上述取消测试验证初始化中的真实连接；不据此声称解析音频 URL 和硬件播放期间的全部竞争场景已验收 |

初始化 worker 在执行用户脚本前，控制器已保存可跨线程终止的句柄。停用、删除、切换与关闭可立即终止初始化进程；IPC 等待不持有子进程锁。完整控制器测试没有使用硬件播放，也没有自动操作原生文件选择对话框。

### 运行预算与边界

| 边界 | 当前值 |
| --- | --- |
| 静态导入校验子进程期限 | 5 秒，不执行脚本 |
| 单次初始化/源请求运行时总期限 | 70 秒 |
| 初始化/源请求 IPC 等待期限 | 75 秒；期间可以主动取消并终止 worker |
| 单个 HTTP 请求默认/最大超时 | 60 秒 |
| JavaScript 微任务预算 | 微任务执行累计 5 秒及 4,096 个 jobs；同步脚本另有循环/递归限额，子进程受 IPC 硬期限约束 |
| 脚本 / HTTP 请求体 | 各 1 MiB |
| HTTP 响应 / IPC 单条结果 | 各 2 MiB |
| Windows worker Job | 512 MiB，最多一个进程，关闭 Job 时终止进程 |
| 持久化 JSON 文件 | 8 MiB |

微任务预算不会抢占所有同步 JavaScript 计算；同步执行由循环/递归限额与子进程的外层 IPC 硬期限共同约束。这些限制由现有代码和边界回归验证；完整 512 MiB 内存耗尽及最长期限强制终止压力测试仍未运行。HTTP 能力及不支持的 LX API 以 [README](../README.md) 和 [固定上游契约记录](upstream-contract.md) 的当前清单为准，不能据此声称任意 LX 源完全兼容。

### 新版独立验收与交付核对

| 0.2 项目 | 最终状态 | 最终证据与边界 |
| --- | --- | --- |
| 最终 EXE 命令行诊断 | BLOCKED，退出码 2 | [新版诊断原始结果](diagnostics.json)；音源与网络步骤均 PASS，真实默认音频输出设备启动失败 |
| 原生 Windows DirectX 截图 | PASS，单独执行 1 项 | [library.png](screenshots/v0.2/library.png)、[sources.png](screenshots/v0.2/sources.png)、[queue.png](screenshots/v0.2/queue.png) 均为真实原生窗口帧，尺寸 1536 × 1095 |
| 0.2 最终打包与启动帮助 | PASS | [bin/lx-gpui-player.exe](../bin/lx-gpui-player.exe)，版本 0.2.0，72,920,576 bytes；实际 `--help` PASS |
| 默认设备硬件播放 | BLOCKED | 当前执行会话未能取得默认音频输出端点；不能判断整台电脑没有声卡 |
| 硬件播放/暂停/进度/切歌与 404 后恢复 | NOT RUN | 需要可用音频输出端点；内存 PCM 和命令派发通过不能替代这一验收 |

最终 0.2.0 EXE SHA256：`85FDC4D738FC161A47DFE2DB3BF7D32D81DB3CCBC2030BDCEE3A5C04B0E3C7D5`。初版产物独立保留，其哈希不能用于校验新版 EXE。

[新版诊断](diagnostics.json) 直接运行最终打包 EXE，分阶段结果如下：

| 阶段 | 结果 | 实际证据 |
| --- | --- | --- |
| 静态导入 / 损坏脚本 | PASS | 隔离语法检查；合法 `throw` 不执行且保持未批准，损坏脚本被拒绝 |
| 初始化与进程隔离 | PASS | 实际同工程子进程初始化，Windows Job 附加成功 |
| `lx.request` | PASS | 真实回环 HTTP POST `/resolve`，显式 header 与 JSON body、callback、展平 legacy 字段 |
| `lx.utils` | PASS | UTF-8/base64 Buffer、MD5、AES-128-CBC、zlib deflate/inflate round trip |
| 取消 | PASS | 幂等终止，已取消 worker 不能继续解析 URL |
| 重启恢复 | PASS | 脚本、政策版本 2 的批准、选择和音量恢复 |
| 硬件音频输出 | FAILED | `Could not find any output device`；播放时钟未启动 |
| 硬件控制与 404 后恢复 | NOT RUN | 输出端点启动失败后未执行后续硬件播放阶段 |

复验新版时，使用可访问默认音频输出端点的会话与新的空数据目录：

```powershell
.\bin\lx-gpui-player.exe --diagnose --data-dir PATH_TO_NEW_EMPTY_DIRECTORY
```

新版三张截图另以版本名称保存到 ChatGPT Library，逐项创建成功并经 Library 列表回读确认；初版文件保留。可使用以下 Library 下载链接，身份、版本和原始返回记录见 [library-files.json](screenshots/v0.2/library-files.json)。

| 页面 | Library 文件 | 大小 |
| --- | --- | --- |
| 曲库 | [lx-native-v0.2-library.png](https://chatgpt.com/api/library/files/libfile_a8e10db53bd88191aca9d968fed001da/download) | 125,118 bytes |
| 音源 | [lx-native-v0.2-sources.png](https://chatgpt.com/api/library/files/libfile_c23cc977b26081919051d6fa1c92147a/download) | 204,868 bytes |
| 队列 | [lx-native-v0.2-queue.png](https://chatgpt.com/api/library/files/libfile_f3e8fb99a0a48191b3b95e283d1962f6/download) | 116,151 bytes |

Windows 不提供官方属性助手使用的 `os.setxattr`，本地 POSIX xattrs 未写入；这不改变上述 Library 创建与回读结果。
