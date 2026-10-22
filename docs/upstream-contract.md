# LX Music Desktop 自定义源兼容契约核对

核对日期：2026-10-04。本文基于官方仓库源码的固定提交，不运行任何第三方音源脚本，不读取用户 LX 配置或凭证。

## 参考版本与来源

- 官方仓库：<https://github.com/lyswhut/lx-music-desktop>。
- 固定提交：`ad95d5091c9ed689fa72b5e5c849df65f5a679ce`，提交时间 `2026-09-19T09:57:31+08:00`，提交说明“发布 v2.12.6”。
- 本地浅克隆：`reference/lx-music-desktop`，已切换到上述提交的 detached HEAD。未改动其源码。
- 官方编写文档：<https://lxmusic.toside.cn/desktop/custom-source>。文档会变动；以下精确行为以固定提交源码为准。官方网页的示例存在歌词字段拼写/调用错误，移植不得照抄这些错误。
- 关键固定源码：[元数据与脚本存储](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/main/modules/userApi/utils.ts)、[运行时桥接](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/main/modules/userApi/renderer/preload.js)、[运行窗口](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/main/modules/userApi/main.ts)、[请求队列](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/main/modules/userApi/rendererEvent/rendererEvent.ts)、[管理生命周期](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/main/modules/userApi/index.ts)。

## 导入、元数据与验证

脚本是 UTF-8 JavaScript，官方文档允许 ES6+。文件头常见写法为 `/**` 块注释，其中独立行含 `* @name ...` 等字段。`utils.ts:53–88` 的实现规则如下：

| 字段 | 值的字符上限 | 缺省值 |
| --- | ---: | --- |
| `name` | 24 | 生成 `user_api_` 加本地时间的名字 |
| `description` | 36 | 空字符串 |
| `version` | 36 | 空字符串 |
| `author` | 56 | 空字符串 |
| `homepage` | 1024 | 空字符串 |

超过上限时取前 N 个 JavaScript UTF-16 code units，再加 `...`。未知 tag 被忽略；重复 tag 以后者为准；值做 `trim()`。精确的字段行正则是 `^\s?\*\s?@(\w+)\s(.+)$`，每个星号前后的可选空白只有一个。头注释正则从 offset 0 匹配 `/* ... */`；前置空白/BOM 会被拒绝。这些是上游解析细节，移植可接受常见 BOM/空白，但应标明更宽松行为。

导入阶段只解析头部、压缩脚本、检查重复并保存，不求值 JavaScript、不验证 JS 语法、不测试 HTTP，也不激活源。仅选中文件或保存元数据不能证明源能运行；源兼容性必须经过单独、经用户许可的初始化与请求验证。

脚本原文经 zlib deflate 后保存成 `gz_` 加 base64。完全相同的压缩脚本会被拒绝为重复，名字相同并不等同于重复。记录 ID 生成形如 `user_api_<随机3位>_<毫秒时间>`，新导入 `allowShowUpdateAlert=true`。UI 提示最多 20 个，实际 UI 条件是 `length > 20`，后台不限制数量；移植应实现自己的明确上限，不把该边界缺陷当作兼容要求。

固定 UI 来源：[本地导入/删除 UI](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/renderer/views/Setting/components/UserApiModal.vue#L67)。

## `globalThis.lx` 与初始化

上游暴露 `lx.version = '2.0.0'`、`lx.env = 'desktop'`，以及 `currentScriptInfo` 的 `name/description/version/author/homepage/rawScript`。`rawScript` 是解压后的原始脚本，不是压缩存储文本。

`EVENT_NAMES` 只有 `request`、`inited`、`updateAlert`。`lx.on(eventName, handler)` 返回 Promise，只允许注册 `request`。只保存一个 request handler，后注册覆盖前者。没有 `off`、`removeEvent`、多监听器或 shutdown 事件；源码里的移除事件函数被注释。

`lx.send(eventName, data)` 返回 Promise，只允许发送 `inited` 和 `updateAlert`。初始化必须恰好一次：

```js
lx.on(lx.EVENT_NAMES.request, ({ source, action, info }) => {
  // 根据 source/action/info 返回 Promise。
  return Promise.resolve('http://127.0.0.1:8765/audio.wav');
});
lx.send(lx.EVENT_NAMES.inited, {
  sources: {
    local: { name: 'Fixture', type: 'music', actions: ['musicUrl'], qualitys: [] }
  }
});
```

上面仅说明契约，不在研究任务中运行。首个合法 fixture 可由项目自建合法合成音频并用本地 HTTP 服务提供。

| 源 key | `type` | 保留下来的 actions | 保留下来的 `qualitys` |
| --- | --- | --- | --- |
| `kw`, `kg`, `tx`, `wy`, `mg` | 必须是 `music` | `musicUrl` | `128k`, `320k`, `flac`, `flac24bit` 的交集 |
| `local` | 必须是 `music` | `musicUrl`, `lyric`, `pic` 的交集 | 空数组 |

其他 source key 被忽略；尽管代码 `supportActions` 中还出现 `xm`，它不在 `allSources` 中，实际初始化不会接受 `xm`。其他 type 被忽略。`name` 在运行时归一化结果中被丢弃，不应依赖它被传回。`actions`、`qualitys` 缺少可用的 `includes` 方法会导致初始化失败；上游未做完整 schema 检查，也允许初始化成功但 sources 为空。移植宜要求非空可用能力并显示验证结果。

`openDevTools: true` 请求打开脚本调试工具，它不属于核心兼容必要能力。第一次调用 `inited` 会先设置“已初始化”标记，再做能力校验；即使能力校验失败，第二次发送也被拒绝。初始化前的可信浏览器 `error` / `unhandledrejection` 会转成初始化失败，信息超过 1024 截断；初始化后的这些错误不会再走初始化状态通道。上游没有脚本初始化 deadline，无 `inited` 的脚本可能一直显示初始化中。Rust 实现应增加求值/Promise/job/初始化 deadline 和内存限制，并在兼容说明中记录这是更严格的边界。

## 请求分发与返回值

播放器向脚本回调发送 `handler({source, action, info})`。handler 必须返回 Promise/thenable；源码直接调用 `.then(...).catch(...)`，纯字符串/普通对象会失败。同步 throw、Promise reject、结果验证失败会回复请求错误。

| action | 输入 `info` | 脚本 Promise 的成功值 | 上游结果验证 |
| --- | --- | --- | --- |
| `musicUrl` | `{type, musicInfo}` | 音频 URL 字符串 | 字符串长度 <= 2048，且匹配 `^https?:` |
| `pic`（仅 local） | `{musicInfo}` | 图片 URL 字符串 | 同上 |
| `lyric`（仅 local） | `{musicInfo}` | `{lyric,tlyric,rlyric,lxlyric}` | `lyric` 必须字符串且 <= 51200；其余字段无效则置 null |

歌词的可选长度上限是严格 `<5120`（`tlyric/rlyric`）、`<8192`（`lxlyric`）。文档一处写成 `lryic/tlryic`，应以运行时代码的 `lyric/tlyric` 为准。

平台 source 的 `type` 是声明的音质；local 的 musicUrl 文档约定 `type=null`。当前 renderer 的 lyric/pic 调用还会带 `type:'music'` 的多余字段，脚本不应依赖它。`musicInfo` 是完整 LX 曲目对象，不仅 `{songmid}`，基础字段为 `id/name/singer/source/interval/meta`；`meta.songId/albumName/picUrl`，平台还有音质列表及平台特有 ID，例如 `kg.meta.hash`、`tx.meta.strMediaMid`、`mg.meta.copyrightId`。只传一个虚构 `songmid` 不能声明兼容所有现有源。

固定类型与调用来源：[曲目类型](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/common/types/music.d.ts#L21)、[renderer 回调封装](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/renderer/core/useApp/useInitUserApi.ts#L35)。

主进程用 `requestKey` 关联结果，请求超时 20 秒后以 `Cancel request` 拒绝；重复 key 替换先前请求。播放器主动取消也移除队列并拒绝 Promise，不把 cancel 事件发给脚本，不会自动调用脚本内部 HTTP 取消函数。切换源时 renderer 外层 Promise 以 `source changed` 拒绝。Rust 移植宜用运行代次标识拒绝旧源迟到结果、关闭 runtime 和取消所有宿主请求，不让上一个源污染新源状态。

上游 `^https?:` 校验不是完整 URL 解析。移植可更严格验证 HTTP(S) URL、长度、host、重定向和响应大小；返回 `file:`、`data:` 或任意本地路径不是上游 musicUrl 兼容行为。

### 搜索是否属于自定义源 API

**不属于。** 自定义源没有 `search` action。歌曲/歌单搜索在内置 music SDK 中进行；脚本收到搜索结果的曲目信息后负责 musicUrl 解析。证据：[搜索 store](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/renderer/store/search/music/action.ts#L87) 调用 `music[source].musicSearch.search`；[SDK 聚合](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/renderer/utils/musicSdk/index.js#L62)。

移植首版宜把“合法本地/fixture 曲库搜索”和“音源 musicUrl resolver”分开。若添加脚本 search 扩展，需清晰标为项目扩展，不得称为上游兼容能力；也不得因为运行一个 fixture 就宣称任意 LX 源兼容。

## `lx.request`、辅助能力与安全边界

`lx.request(url, options, callback)` 是 callback API，返回可调用的取消函数。参数与结果：

- options 包含 `method/headers/body/form/formData/timeout`，method 默认 `get`。body 优先于 form，form 优先于 formData。timeout 单位毫秒；正数取与 60000 的较小值，否则默认 60000。
- 宿主使用 Node needle，HTTP 不受页面 CORS 规则限制；可跟随宿主配置的 HTTP proxy。移植不应为了兼容偷偷读取用户代理或凭证。
- 网络失败调用 `callback(err,null,null)`；成功调用 `callback(err,resp,body)`。resp 有 `statusCode/statusMessage/headers/bytes/raw/body`。
- 响应 raw 转成文本后尝试 JSON.parse；成功则 `resp.body` 和第三参数是解析对象，否则是文本。HTTP 非 2xx 本身不会在该桥接层转成 Error，脚本须检查 statusCode。
- 返回取消函数 abort 请求。上游该函数调用后把内部 request 设 null，再次调用可能 throw；移植宜做幂等取消。

辅助 API 精确列表：

| API | 功能 |
| --- | --- |
| `lx.utils.buffer.from(...args)` | Node Buffer.from 对应能力 |
| `lx.utils.buffer.bufToString(buf,format)` | Buffer 字符串转换 |
| `lx.utils.crypto.aesEncrypt(buffer,mode,key,iv)` | createCipheriv + update/final，返回 Buffer |
| `lx.utils.crypto.rsaEncrypt(buffer,key)` | 左补零至 128 bytes 后 RSA_NO_PADDING |
| `lx.utils.crypto.randomBytes(size)` | 加密安全随机 bytes |
| `lx.utils.crypto.md5(str)` | 十六进制 MD5 |
| `lx.utils.zlib.inflate(buf)` | Promise<Buffer> |
| `lx.utils.zlib.deflate(data)` | Promise<Buffer> |

**没有 `lx.storage`、`lx.getItem`、`lx.fetch`、文件系统或任意进程 API。** 桌面脚本执行在浏览器 renderer，具有普通浏览器 JS 环境和 timers；不是 Node 脚本。上游 `contextIsolation=true`、`nodeIntegration=false`、`sandbox=false`；CSP `default-src 'none'`，禁用页面导航、redirect/webview、新窗口和窗口权限。网络由预加载的 `lx.request` 经宿主执行。这个设计不等于任意不可信脚本无风险，移植不能声称绝对安全。

首版可明确不支持加密/Buffer/zlib/DOM/网络扩展，并给出具体缺失 API 错误；不能静默返回空对象。音源静态导入不执行脚本，信任并启用是独立动作。网络能力授权应明确覆盖请求目标与潜在上传，避免导入即授予任意网络。合成 fixture 只能访问项目自建 loopback HTTP 服务；研究和验证不寻找未经授权的第三方音源、不实现付费/DRM 绕过、不上传本地数据。

## 生命周期、持久化和更新

持久化对象是应用管理数据，而非脚本私有存储：`user_api.json` 的 `userApis` 数组包含元数据、id、更新弹窗设置和压缩脚本。加载时把脚本放入内存 map，返回 UI 的列表不含脚本。旧记录缺 version 时尝试重解析，失败移除；缺更新提示标记时设 false。默认内置自定义源数组为空。底层 store 通过临时 JSON 文件 + rename 保存；读取坏 JSON 时备份为 `.bak` 再重建。选择由 appSetting 的 `common.apiSource` 另外保存。

| 用户动作 | 上游行为 |
| --- | --- |
| 导入 | 静态保存，不自动运行、不自动选择 |
| 启用/选择 | 停掉旧窗口，取对应脚本，创建隐藏窗口执行，等待 inited 通知 |
| 停用/切到其他源 | 关闭现有 runtime；没有 shutdown 回调 |
| 删除当前源 | 关闭 runtime，再移除记录与脚本；renderer 选择可用备用源 |
| 删除未启用源 | 仅移除持久记录 |
| 应用关闭 | 清理 runtime |
| 重启 | 读列表与保存的选择，再重新执行所选脚本；不是恢复 JS heap |

`closeWindow()` 清 auth cache、session storage、cache 后 destroy。没有为脚本提供稳定的跨重启 KV；也不能声称浏览器 localStorage 可用或可靠。固定存储/启动证据：[store](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/main/utils/store.ts#L16)、[设置与 source 切换](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/renderer/core/apiSource.ts#L9)、[重启初始化调用](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/renderer/core/useApp/useInitUserApi.ts#L173)。

`updateAlert` 每次 runtime 只允许一次，log 必填字符串，超过 1024 截断加 `...`，updateUrl 可选；用户标记 `allowShowUpdateAlert` 控制是否显示。它只提醒、可经用户点击打开页面，**不自动下载/更新/执行脚本**。上游 updateUrl 校验逻辑用 `&&` 导致部分无效或过长 URL 留下；Rust 应实现严格 HTTP(S) 校验。

## 许可证与派生记录

根 [LICENSE](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/LICENSE) 是 Apache License 2.0，package.json 的 license 字段也为 Apache-2.0。与此同时，[README 项目协议](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/README.md#L115) 与 [中文补充协议](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/licenses/license_zh.txt)、[英文译文](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/licenses/license_en.txt) 明示这些补充内容冲突时优先。补充协议描述技术探索/研究及不接受商业合作/广告/捐赠、合法使用/版权保护、版权数据 24 小时内清除等。英文是中文译文，存在实质翻译差异：中文 §1.1 表示不对数据合法性、准确性负责，英文写为负责，未找到明确语言优先条款。license.rtf 的文本规范化后与中文 TXT 相同；上游打包配置会附带整个 licenses 目录，NSIS 引用 RTF，AppImage 引用中文 TXT。不能把整个仓库简单描述成“仅 Apache-2.0”，也不能由这个研究结论保证商业使用权限。

若实际复制或改写上游源码，应随项目保留未改写的 Apache LICENSE、原中文/英文/RTF 补充协议与 README 协议段，并在 THIRD_PARTY_NOTICES/来源记录中标明 lyswhut、仓库 URL、固定提交、哪些文件复制/改写及修改说明。Apache §4 的许可证、修改声明、保留相关版权/归属 notices、如有 NOTICE 时保留相关 NOTICE 要求应被落实。该版本全部 tracked 文件中未发现 NOTICE/COPYING/COPYRIGHT 文件，不必虚构“上游 NOTICE”；该结果不覆盖未随仓库发布的运行时依赖。根 LICENSE 的 Copyright 行是 Apache 附录示例，不能据此认定项目版权人。只研究格式并独立实现的部分需准确描述为兼容实现，不虚构“复制自”记录；保留参考与许可文件有助于审计，但本文不裁定 API 格式是否构成衍生作品。

不要直接复制上游封面、logo、字体或其他装饰资源：仓库的 simplify-chinese-main 是 MIT / Copyright 2021 Shigma；snow.min.js 标有 Jam-Es.com MIT 版权头，但其所指根 MIT 全文不在当前根 LICENSE 中；angle-right-solid.svg 带 Font Awesome Pro 6.2.0 商业授权注释。它们不能被根 Apache 标记覆盖。Rust/GPUI/gpui-component 依赖应单独核对其许可证。音乐 URL 被解析或可播放，不代表播放器取得了该音乐版权。

## 首版兼容声明与必要验证建议

实现报告应逐项区分“已实现并验证”“已实现未验证”“未支持”。建议首版通过的闭环是静态导入 → 显示元数据 → 用户信任并启用 → inited 能力校验 → 曲库搜索/选曲 → musicUrl Promise 分发 → HTTP(S) 返回值校验 → 真实解码和播放。至少覆盖损坏/无头注释/语法坏/永不 inited/Promise reject/无 request handler/非法 URL，切换/删除后的迟到响应，源列表与信任状态重启恢复，以及播放失败后重试/切换恢复。

初始 fixture 由项目自行编写，元数据注明测试来源，仅返回自行合成的音频 URL；搜索用项目独立合法 catalog。Buffer/crypto/zlib、updateAlert、lyrics/pic、代理、DOM、平台曲目元数据、完整针对此契约的 HTTP 表单能力若未实现，都必须列在剩余兼容清单里。不能称“任意 LX 音源完全兼容”。

## 扩展契约补充：网络、字节工具与异步取消

本节是 2026-10-04 扩展实现的核对依据及验收建议，**不是测试通过声明**。0.2 实际已执行的检查、82 项常规测试与仍受阻的硬件验收，见[验证记录](validation.md#02-权限与网络能力扩展验收)。除固定 LX 桥接源码外，额外核对了其真正锁定的 HTTP 依赖：`package.json:182` / `package-lock.json:9831` 使用作者维护的 `lyswhut/needle` fork，commit `93299ac841b7e9a9f82ca7279b88aaaeda404060`（package version 3.2.0）。相关永久来源：[needle.js](https://github.com/lyswhut/needle/blob/93299ac841b7e9a9f82ca7279b88aaaeda404060/lib/needle.js#L267)、[querystring.js](https://github.com/lyswhut/needle/blob/93299ac841b7e9a9f82ca7279b88aaaeda404060/lib/querystring.js)。不能用注册表最新 Needle 文档推断此 fork 的全部行为。

### HTTP 数据与响应精确语义

LX 仅转发 `method/timeout/headers/body/form/formData`，不转发任意 Needle 参数。payload 选择用 JavaScript truthiness：truthy `body` 优先，其次 truthy `form`、truthy `formData`；`''/0/false/null/undefined` 的 body 会落到下一分支，空对象/空数组仍为 truthy。form 和 formData 均设置 `json=false`；桥接没有设置 `multipart=true`，所以 **formData 名称不代表 multipart 上传**。

| 请求情况 | 固定 Needle fork 的行为 |
| --- | --- |
| 字节 Buffer payload | 直接作为请求 body，不再做 JSON/form 编码 |
| POST 字符串 payload | 按 UTF-8 发送原文；无显式 Content-Type 时默认 application/x-www-form-urlencoded |
| POST 对象 body，Content-Type 精确为 application/json | JSON.stringify；`application/json; charset=utf-8` 不触发这个精确匹配 |
| POST form/formData 或普通对象 body | urlencoded；值空格编码为 `%20`，数组 key 使用 `[]`，嵌套对象 key 使用 `[child]`，null 值编码成空值 |
| GET 非 Buffer、非 JSON payload | 把 payload 编成 query；替换原有 query，而非追加；该兼容差异应单独验证或声明 |
| GET JSON 对象 body | 可以发送 JSON body；只实现无 payload GET 的端口不得声称全量兼容 |

headers 最终名称转成小写，用户给定值覆盖 Needle 缺省值。timeout 传入的是 response_timeout：默认/上限 60000 ms；fork 的 open_timeout 默认为 10000 ms，收到响应 headers 后 read_timeout 默认为 0，因此上游的 60 秒不是完整下载的总 deadline。Rust 可以设置更严格的连接/整请求 deadline，但需记录，不应保持无界下载。

fork 默认 `follow_max=0`，因此不跟随重定向，但 3xx、4xx、5xx 都可以正常得到 `callback(null,resp,body)`。端口可以禁用 redirect follow，同时仍保留 3xx 响应，以避免把普通状态码误报为网络失败。`resp.raw` 为收到的原始 response payload bytes（HTTP chunk framing 已由 HTTP 层去除），`resp.bytes` 为这些字节的计数。fork 虽有解压/charset/JSON 管道，LX 桥接再次用 `raw.toString()` 覆盖 body，并尝试 JSON.parse；故 raw 不应被自动 gunzip，body 不应按 Content-Type 额外强制解析。gzip 等返回内容如果要解析，应由脚本调用其字节工具。HTTP statusMessage 是服务器 reason phrase；基于 HTTP/2 或库无法取得时用标准 reason phrase 属近似兼容。重复 response headers、特别是 set-cookie，在上游可为数组；仅保留一个字符串值属于子集兼容。

受控外网能力须绑定到确切脚本内容的许可，使用正常证书验证与受信根，限制方法、URL、headers、请求/响应大小、并发数、整体 deadline。禁用自动用户代理配置、cookie jar、系统凭证与文件/进程访问。重定向若实现，每跳重新验证目标及网络授权；DNS 地址检查和实际连接必须一致，避免解析后重绑定或 public host 跳入私有网段。上游的宽松 URL、隐式认证和无响应容量上限不应照搬为安全边界。

### Buffer、MD5、AES 与 zlib

上游将相关参数直接交给 Node 内置函数。Node 官方语义参考：[Buffer](https://nodejs.org/api/buffer.html#buffers-and-character-encodings)、[Crypto](https://nodejs.org/api/crypto.html#cryptocreatecipherivalgorithm-key-iv-options)、[zlib](https://nodejs.org/api/zlib.html#zlibdeflatebuffer-options-callback)。首版应枚举实际支持的输入形式、编码、模式并针对错误报清晰信息，不能因为实现了常见 AES-CBC 就宣称所有 Node/OpenSSL 算法兼容。

- `buffer.from(string,encoding)` 默认 UTF-8；Node 的常见编码包括 utf8/utf-8、utf16le/utf-16le/ucs2/ucs-2、latin1/binary、ascii、hex、base64/base64url，编码名不区分大小写。UTF-8 坏字节解码为 U+FFFD；latin1 输入取 UTF-16 code unit 的低 8 位；ascii 输入同 latin1，输出额外清最高位。hex 在首个无效字符或尾部单独半字节处截断；base64 可读 URL 字母表并忽略空白，base64url 输出省去 padding。未实现的宽松解码行为应标明。
- `buffer.from(array)` 按字节转换，Buffer/Uint8Array 输入复制；ArrayBuffer+byteOffset+length 在 Node 共享原始内存。`bufToString(buf,format)` 精确为 `Buffer.from(buf,'binary').toString(format)`：string 输入先按 latin1 转 bytes，format 决定输出编码，未传 format 默认 UTF-8。
- `md5(data)` 输出小写 32 位 hex；string 按 UTF-8，Buffer/TypedArray/DataView 按 bytes。普通 JS numeric array 不是 Node Hash.update 的合法输入；应先 buffer.from。端口若宽松接受数组，属于扩展。
- `aesEncrypt(data,mode,key,iv)` 直接使用 createCipheriv，不派生/哈希 key。data/key/iv 的 string 按 UTF-8；CBC key 为 16/24/32 bytes，iv 必须 16 bytes；ECB iv 为 null 或零长度。CBC/ECB 使用默认 PKCS#7，空输入及整块输入也追加一个 padding block；返回 bytes，编码由 bufToString 决定。CTR 等流模式没有 CBC/ECB 的 padding。RSA 可以明确 unsupported，不能返回伪造成功值。
- zlib.inflate/deflate 接受 bytes（Node deflate 也接受 UTF-8 string），返回 Promise<bytes>；格式是带 zlib header/Adler-32 的 DEFLATE，非裸 DEFLATE/gzip。压缩 bytes 可随 zlib 版本不同，验收应检查标准互解/还原原文，不把某一次压缩结果作为唯一合法字节序列。解压必须在累计输出达到容量上限前停止，不能先解压全部再检查长度。

### 异步取消审查结论

扩展前 `source_bootstrap.js` 在 Promise job 中同步调用 native_http，再由 reqwest::blocking 等待网络。JavaScript 主线程此时被占用，返回的 cancel closure 无法在进行中的请求期间执行；布尔 flag 只证明发送前取消。真正的 in-flight cancel 需要 start/poll/cancel 或相当的异步桥接：宿主线程只返回可序列化结果，Boa 对象与 callback 始终留在所属 JS 线程；事件泵轮流处理网络完成与 Promise jobs。

每个 HTTP request 应有独立 token，以及启用源的 generation。cancel 原子移除/标记 token、终止其宿主 I/O，回收队列，不再投递 callback；完成与 cancel 竞争时仍最多一次。停用/删除/切源需要取消该 generation 的所有 I/O，并丢弃迟到结果。只“丢 callback”但一直占用网络线程不是完整取消；仅杀整个 worker 可以取消源，但不能当作脚本 request 的独立取消实现。controller 已有 generation 过滤及 worker kill 边界，可保留为第二道控制。

扩展方案的 `HttpBridge::begin_operation()` 会取消上一个 operation 未完成的网络任务；因此不支持跨 resolver 请求长期保留后台 HTTP，这是更严格的子集，依靠后台 HTTP 刷新缓存的脚本需要适配。完成队列也应有独立容量限制：仅限制当前 pending 数量不足以限制“已完成、未 poll、随后取消”的 stale payload 总量；cancel storm 不能积累大量已被丢弃的响应。使用有界、非阻塞投递或活跃 token 过滤，避免网络任务阻塞在发送完成消息上。

HTTP 等待预算、初始化/resolve deadline、Promise job CPU budget 与 worker IPC deadline 必须协调。扩展前均有约 5 秒预算；若 HTTP 允许较长 timeout，不能在正常网络等待期间误触发 5 秒 Promise CPU 限制，也不能移除阻止死循环的限制。事件泵应有独立 wall-clock operation deadline，pending I/O 可等待而 CPU/jobs 仍受限；跨线程不得传递 JsValue/JsObject/Rc<Context>，不得持有会阻止 cancel 的互斥锁。

### 扩展验证对照表（建议，不代表已运行）

| 合成/本地测试 | 必须观察的结果 |
| --- | --- |
| GET JSON、JSON primitive、非 JSON text | callback 的 body 与 resp.body 一致；raw 保留 bytes；JSON null/true/number 不被误判 |
| 302/404/500 | 不跟随 redirect；err=null 且 status/raw/body 可读 |
| POST text、对象 JSON、form，body/form 同时存在 | 请求原文、Content-Type、truthy 优先级正确；`body:''` 可回落到 form |
| Unicode/空格/数组/null/nested form | UTF-8 percent 编码及上游 bracket 规则正确或明确报 unsupported |
| 原 URL 带 query 的 GET form | 原 query 被上游规则替换，或记录移植的显式差异 |
| gzip/非法 UTF-8 响应 | raw 不透明解压；bytes 为 wire payload 长度；text 解码有 replacement |
| 超大 declared length / chunked / 解压 bomb | 在读取/累计输出阶段限额，正常错误 callback，无无界内存增长 |
| TLS 不受信证书 / 连接失败 / 慢 headers / 慢 body | 不关闭证书校验；错误 callback(err,null,null)；deadline 实际生效 |
| 发送前双 cancel / 进行中 cancel / 完成取消竞态 | 幂等，取消 I/O，callback 为零或已完成的一次，无二次回调 |
| in-flight 时 disable/delete/switch | 旧 callback/result 被丢弃；新源和播放状态不被旧结果修改 |
| utf8/latin1/ascii/hex/base64 round trip | bytes 与输出编码正确；坏编码及边界行为有清楚结果 |
| MD5 UTF-8 string / Uint8Array | 标准摘要，如 abc 为 900150983cd24fb0d6963f7d28e17f72 |
| AES-CBC/ECB 128/192/256，空与整块 data | 标准加密结果，正确 key/IV/padding；非法长度、mode 明确报错 |
| zlib round trip / 固定标准数据 / malformed | Promise resolve bytes 或 reject Error；不是同步伪 Promise 成功 |
| 并行 HTTP + Promise chaining + 连续重启 | JS callback 仅在 runtime 线程，队列有界且没有线程/handle 泄漏 |
