# 同用户单实例 DeepLink 转发

Windows 后端可以显式启动同 profile 的 DeepLink 转发服务。普通 `BackendHandle::start` 和 CLI 默认关闭此服务。长驻 CLI 的启动方式：

```powershell
lx-native-cli --data-dir C:\MusicProfile --audio memory --instance-relay --json-lines
```

保持标准输入打开，使用 JSON Lines 查询和控制后端。第二个 CLI 进程可向持有同一 profile 写锁的服务转发：

```powershell
lx-native-cli --data-dir C:\MusicProfile deeplink lxmusic://player/pause
lx-native-cli --data-dir C:\MusicProfile --command '{"request_id":"pause-001","command":"dispatch_deep_link","url":"lxmusic://player/pause"}'
```

位置参数 `deeplink` 为每次调用生成有界的随机 request ID；JSON 中显式提供的 ID 原样保留。后端以同一 ID 和同一输入重放已有回执，同一 ID 对应不同输入时返回拒绝回执。转发进入普通 `DispatchDeepLink` actor 分支，origin 为 `instance_relay`。转发只携带 request ID 与 DeepLink；不携带任意后端命令或调用者权限。

CLI 在取得 profile 锁之前验证 DeepLink、请求 ID 和 origin。只有实际锁竞争、实际保留的 profile 描述符以及 `expected_revision` 为空的 DeepLink 才可以转发。带 revision 条件的第二实例请求明确失败；查询和其他命令不转发。第二实例不打开另一个 Repository，也不启动另一个写入后端。

回执包含 `request_id`、真实 actor `operation_id` 和 `disposition`。`accepted` 表示异步操作已经接受，CLI 返回 `completed:false`。`applied`、`committed` 和已有终态回执 `completed` 返回 `completed:true`。异步操作的成功或失败仍应通过长驻实例的事件和结果查询确认。`rejected`、`busy` 和 `cancelled` 是负回执；它们不承诺命令执行之前的状态已恢复。网络断开或回执超时可能发生在命令接受之后，错误会标明交付未确认，重试前应查询同一 request ID。

管道名称由实际验证的 profile 锁文件身份与当前用户 SID 派生。服务端必须持有写锁；客户端使用锁竞争失败时保留的实际描述符重新验证对象身份。Windows 管道使用首实例标志、拒绝远程客户端以及仅当前 SID 的 DACL，连接两端还核对对端进程用户 SID。请求使用有界 UTF-8 帧，DeepLink 上限为 32768 字节，request ID 和 operation ID 上限为 128 字节。全进程最多持有 4 个真实 relay worker 配额，包含退休 worker；当前后端启用一个服务连接。I/O 和回执观察期限最多 10 秒。

原有 Source 批准、媒体 URL 批准、本地文件授权和 DeepLink 解析规则继续生效。转发服务不会注册 `lxmusic://` 协议处理器、修改 Registry、启动 GUI 或激活窗口。GUI 控制器的命令行 URI 接入与窗口激活尚未接线。

正常退出关闭输入接收，取消挂起 I/O，等待 OVERLAPPED 的真实终态并关闭实际句柄，再 join 服务、客户端和退休 worker。超时保留实际 worker 所有者，继续观察和重试退休；最后一项真实所有权退出后，退休 worker 实际退出、join，并可在后续启动时重建。CLI 客户端观察超时后先取消并等待实际退休，句柄关闭异常时清理可能超出回执观察期限。

启动失败的已附着 Runtime 通过有界 startup keeper 保留实际资源和 profile 锁，直至服务实际 join。actor 裸线程 panic 时的完整资源交接与连续 profile 锁保持仍待实现；正常退出成功不能作为该故障路径的证明。完整物理 profile 恢复交接仍是独立的生命周期契约。

隔离测试覆盖实际 profile 写锁竞争、真实 Backend actor 回执、异步 `accepted` 到幂等 `completed`、拒绝回执、CLI 第二实例与 CAS 拒绝，以及正常退出后的同 profile 重启。原生 transport 的独立管道测试需要显式执行 ignored fixture。所有测试使用独立临时 profile／管道，不操作现有用户 profile 或协议 Registry。
