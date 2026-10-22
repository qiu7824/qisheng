# LX Native 0.3.0-alpha.2 后端检查点

日期：2026-10-06。平台：Windows x64/MSVC，Rust/Cargo 1.99.0，数据库 schema 4，后端协议 1。

## 可运行范围

独立后端、SQLite、本地文件与四轨歌词链路延续 alpha.1；五平台 SDK 提供活动搜索、热词、详情、榜单目录与歌曲、歌单、评论、歌词及封面操作。ProviderRequest 异步运行，支持结果查询、取消、期限、聚合结果预算和关闭清理。内置 SDK 保持与已批准脚本的传输权限边界。

原生 KRC/QRC/MRC 与 KW/WY 动态歌词转换进入平台和本地读取链路。源工具支持完整 Buffer 方法面、受限 RSA_NO_PADDING、定时器和类型化 host event。列表、Dislike、MRU 搜索历史事务仓与五模式队列纯规则已建立；此检查点尚未将新队列、在线列表及内置在线歌词路由接入播放服务。

## 验证证据

| 检查 | 结果与范围 |
| --- | --- |
| 无界面全部目标测试 | 完整运行 33 个测试套件，295 项通过，0 项失败，1 项物理输出设备测试跳过 |
| SDK 协议与密码算法 | 五平台请求、签名、分页、ID/质量、响应规范化、取消与故障 fixture；WY 公钥来自固定基线并由独立 Node/OpenSSL 验证 |
| 原生歌词 codec | 独立供应商格式 fixture 验证 KRC 四轨、QRC 变体及 XML 原始换行、MRC 有符号运算与词时间 |
| 源工具兼容 | 独立 Node 2129 项 Buffer 差分通过；实际 worker、RSA、timers 和 host event 回归通过 |
| 官方只读样例 | 五平台各 7 类样例路径，共 35 个调用已验证；完整歌单、歌词、榜单歌曲及全部分页未逐项联测 |
| 格式与静态检查 | 格式检查、无界面全部目标严格 Clippy 通过；默认 GPUI 特性全部目标编译检查通过 |
| 正式开发 CLI | 版本、五平台能力查询、独立 worker 静态源导入、关闭与未授权状态烟测通过 |

全部测试总数仅统计统一 Cargo 运行，不重复累加专项测试和外部 Node 差分。Memory PCM 不替代真实声卡输出；SDK 响应元数据不替代实际在线音频播放。详细活动能力与联测边界见[SDK 状态](sdk-status.md)。

检查点统一验收状态：通过。完整后端状态：未完成。

## 开发产物

文件：[lx-native-cli-0.3.0-alpha.2.exe](../bin/lx-native-cli-0.3.0-alpha.2.exe)。大小：36,958,720 bytes。

SHA256：`1DC9ADE59C81B8B2319E85B99C43C4726B5C99251E76FED90D1AA72B57FB2E1D`。

CLI 默认使用 Memory 模式。`--audio device` 选择真实输出；`search-online PLATFORM TEXT` 查询在线 SDK；JSON Lines 保持操作期间的查询与取消能力。此文件固定为 schema 4 检查点，后续开发源码及 profile 版本以对应开发状态为准。
