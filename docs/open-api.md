# 栖声 HTTP 接口

在「设置 → 外部接口」开启「LX 兼容 HTTP API」。默认端口为 23330，可以点击端口按钮修改；开关与端口保存在当前数据目录，重启后恢复。端口被占用或启动失败时，设置页显示实际错误。

GUI 接口仅监听 `127.0.0.1`，本机应用可直接请求，无需令牌。关闭开关或退出程序时停止接口并断开订阅。浏览器跨域来源默认不开放。

状态、歌词、播放控制及 SSE 格式兼容 [LX 官方 HTTP API](https://lxmusic.toside.cn/desktop/open-api)。搜索与歌单不是该官方 HTTP API 的路由；歌单导入使用界面的链接导入或文件导入功能。

开发用 JSON Lines 服务入口与 GUI 共用后端控制线程。CLI 服务启动、凭据轮换、局域网地址和浏览器来源配置见 [CLI 服务控制](backend-cli-control.md)。

```json
{"service":"open_api_start","config":{"port":23330}}
```

CLI 启动的受保护端点使用 `Authorization: Bearer TOKEN`，与 GUI 的本机兼容端点区别配置。状态查询不返回凭据；凭据轮换或端点关闭后，旧会话不能继续控制播放器或读取其图片。

| 路由 | 行为 |
| --- | --- |
| `/status` | 当前播放状态；`filter` 选择字段 |
| `/lyric` | 当前普通歌词 |
| `/lyric-all` | 当前四轨歌词 |
| `/play`、`/pause` | 播放或暂停 |
| `/skip-next`、`/skip-prev` | 下一首或上一首 |
| `/seek?offset=秒数` | 跳转到原始媒体时间 |
| `/volume?volume=百分比` | 设置音量 |
| `/mute?mute=true` | 设置静音 |
| `/collect`、`/uncollect` | 修改当前曲目的收藏状态 |
| `/subscribe-player-status` | SSE 状态推送；`filter` 选择事件字段 |

`status.picUrl` 只发布当前曲目已经批准并验证的缓存图片。值为同一端点的相对路径，图片请求沿用端点的 Host、Origin 和认证配置：

```text
/artwork/v1/会话epoch/播放generation/图片键SHA256/正文SHA256
```

图片支持 GET 和 HEAD。每幅图片最多 8 MiB，最多两条图片正文同时传输；图片额度独立于控制 JSON 和 SSE 的原有限额。路径中的标识不能批准新的远程 URL、本地文件或音源。

曲目、播放 generation、源权限、图片键、缓存、TTL 或端点变化会撤销旧图片引用。服务逐段发送已验证的内存数据；图片请求不读取新的磁盘图片，不触发网络获取，不在地址中泄露文件路径、源 URL 或 Bearer 凭据。

图片尚未解析、尚未批准、没有有效缓存或权限已撤销时，`picUrl` 为空。客户端通过已有图片解析和批准接口准备图片，再读取状态或订阅 SSE。

歌单、平台查询、下载、备份和其他业务命令使用 Rust 后端或 JSON CLI 的对应接口。HTTP 层维持播放器控制的 LX 兼容路由和受控图片读取边界。
