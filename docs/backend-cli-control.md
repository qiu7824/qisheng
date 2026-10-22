# CLI 服务控制

JSON Lines 会话以 `--json-lines` 启动，每行输入一个 JSON 对象并接收一行响应。会话可以同时执行播放器命令、读取后端状态和管理 OpenAPI 端点。服务只在显式启动后监听，标准输入结束时关闭端点并等待工作线程退出。

```text
lx-native-cli --data-dir PATH --audio memory --json-lines
```

启动本机端点：

```json
{"service":"open_api_start","config":{"port":23330}}
```

默认生成 256 位 Bearer 凭据，成功启动的响应返回 `address`、`session_epoch` 和 `bearer_token`。后续状态查询不返回凭据。HTTP 调用使用 `Authorization: Bearer TOKEN`；凭据不会授予音源脚本批准、音频 URL 批准或本地文件读取权限。

指定已有凭据时，`bearer_token` 必须是 64 个十六进制字符。`port: 0` 使用系统分配的端口，实际地址由响应返回。

```json
{"service":"open_api_start","config":{"port":0}}
{"query":"open_api"}
{"service":"open_api_rotate","config":{"port":23330}}
{"service":"open_api_stop"}
```

轮换先校验新配置，再撤销旧会话、关闭连接并等待端点退出。有效的新端点使用新的会话 epoch。旧凭据和旧会话请求不再生效。一个请求只能包含一种查询、播放器命令或端点服务操作。

本机兼容模式需要显式设置 `legacy_loopback: true`，不能同时提供 Bearer 凭据或启用局域网监听：

```json
{"service":"open_api_start","config":{"port":23330,"legacy_loopback":true}}
```

局域网监听需要 `bind_lan: true`、Bearer 鉴权和 `allowed_lan_hosts` 中列出的本机接口 IP。浏览器来源必须通过 `allowed_origins` 列出准确的 HTTP Origin；来源值不包含路径、查询参数、片段或账号信息。

```json
{"service":"open_api_start","config":{"port":23330,"bind_lan":true,"allowed_lan_hosts":["192.168.1.10"],"allowed_origins":["https://controller.example"]}}
```

OpenAPI 启停及状态查询只在当前 JSON Lines 会话中生效，不写入自动启动设置。未启用端点的后端启动不创建该监听器。

播放器深链通过后端命令入口调用：

```json
{"request_id":"player-pause","command":"dispatch_deep_link","url":"lxmusic://player/pause"}
```

深链中的远程曲目只作为经过校验的音乐元数据进入临时列表。播放继续使用现有音源执行和精确音频 URL 批准流程；深链中的未知文件路径、批准标记和音频 URL 不产生授权。
