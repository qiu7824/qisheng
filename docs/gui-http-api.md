# 栖声 HTTP API

在「设置 → 外部接口」开启「LX 兼容 HTTP API」，并查看监听地址。默认地址为 `http://127.0.0.1:23330`；端口按钮可修改端口。开关和端口会随当前数据目录保存。关闭接口或退出程序后，监听与订阅连接一并停止。

接口供本机应用调用，无需令牌；不监听局域网，默认不开放浏览器跨域来源。端口被占用时，设置页显示错误，可以选择其他端口。

## 路由

全部使用 HTTP GET。

| 地址 | 功能 |
| --- | --- |
| `/status` | 当前播放状态，返回 JSON |
| `/status?filter=status,name,singer,progress,duration` | 按字段筛选状态 |
| `/lyric` | 当前歌曲 LRC 歌词，返回 UTF-8 文本 |
| `/lyric-all` | 普通、翻译、罗马音及扩展歌词，返回 JSON |
| `/subscribe-player-status` | SSE 实时状态订阅，支持 `filter` |
| `/play`、`/pause` | 播放、暂停 |
| `/skip-next`、`/skip-prev` | 下一曲、上一曲 |
| `/seek?offset=30.5` | 跳转到指定秒数 |
| `/volume?volume=50` | 设置音量百分比，范围 0–100 |
| `/mute?mute=true`、`/mute?mute=false` | 设置静音 |
| `/collect`、`/uncollect` | 收藏、取消收藏当前歌曲 |

## 状态字段

可筛选字段为 `status`、`name`、`singer`、`albumName`、`picUrl`、`duration`、`progress`、`playbackRate`、`lyricLineText`、`lyricLineAllText`、`lyric`、`tlyric`、`rlyric`、`lxlyric`、`collect`、`volume`、`mute`。

`status` 使用 LX 的兼容值 `playing`、`paused`、`error`、`stoped`。默认返回状态、歌曲名、歌手、专辑、当前歌词、时长、进度和播放速率。

`picUrl` 为当前已经批准并验证的封面缓存引用；可能为空。相对路径使用当前 API 地址作为基址。切歌、清理缓存、权限变化或关闭接口后，旧封面地址可能失效。

## 调用示例

```shell
curl "http://127.0.0.1:23330/status"
curl -N "http://127.0.0.1:23330/subscribe-player-status?filter=status,name,progress"
curl "http://127.0.0.1:23330/pause"
```

SSE 每个字段使用独立事件名，例如 `event: status` 和 `data: "paused"`。连续显示歌词或进度时优先订阅，避免高频轮询。

路由与字段参照 [LX 官方 HTTP API](https://lxmusic.toside.cn/desktop/open-api)。歌单导入位于栖声的「歌单」菜单；搜索和歌单不是上述官方 HTTP 接口的路由。
