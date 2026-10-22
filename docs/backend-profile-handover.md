# 连续 profile 锁交接

`BackendHandle` 在关闭服务、退出 actor 和关闭 SQLite 期间保留同一原生 profile 锁。Rust 调用方通过不可序列化的 `ProfileQuiesceTicket` 和 `ProfileGuard` 交接拥有者；路径、查询结果和超时均不能重新创建该权限。

| 接口 | 行为 |
| --- | --- |
| `begin_profile_quiesce(operation_id, expected_revision)` | 关闭修改与外部服务接入，提交实际 revision 校验的交接请求 |
| `profile_quiesce_state()` | 读取当前交接阶段、actor join 和错误 |
| `retirement_state()` | 读取实际 actor 与 Runtime 退休状态 |
| `ticket.observe(wait)` | 重复观察同一请求，单次观察最多 120 秒 |
| `ticket.take_handover(wait)` | 全部实际退出并关闭 SQLite 后，取得 checkpoint seal 与原 `ProfileGuard` |
| `ticket.take_failed_guard(wait)` | 失败路径全部实际关闭后取回原锁，保持原交接失败结果 |

```rust
let (snapshot, _) = backend.snapshot_with_cursor()?;
let ticket = backend.begin_profile_quiesce("profile-transfer".into(), snapshot.revision)?;
match ticket.take_handover(std::time::Duration::from_secs(15)) {
    Ok(handover) => {
        handover.guard.verify()?;
        let restarted = BackendHandle::start_with_guard(config, handover.guard)?;
        // restarted owns the same writer acquisition.
    }
    Err(failure) => {
        // Retain failure.error and its owner; observation expiry is not closure.
        let view = failure.owner.observe(std::time::Duration::from_secs(1))?;
    }
}
```

交接等待音频、目录权限与实际文件句柄、歌词图片、Catalog、下载、清理、快照、系统媒体、DeepLink、同步和外部端点的真实退休与 join。SQLite 的 WAL checkpoint 和 connection close 失败返回原实际 Repository/连接/Guard，重试使用同一拥有者。

actor panic 只展开借用 Runtime 的执行作用域。预先保留的生命周期槽继续持有整个 Runtime、实际 actor 句柄、Repository 和文件退休屏障；观察方退出不会提前释放锁。实际工作线程 panic 保留首次错误，服务继续关闭；最终阶段为 `failed_guard`，普通交接和关机仍返回失败。调用方可在全部物理退出后显式取回原锁，用于原 profile 的恢复或重启。

全进程最多四个活跃或尚未退休的 Backend 生命周期。容量不足时新启动失败，不创建第五个未受监督 Runtime；传入的原锁由 `RetainedProfileGuardStartupFailure` 保留。Repository 打开失败使用 `RetainedRepositoryOpenFailure`；打开之后的初始化失败使用 `RetainedRepositoryInitializationFailure` 保留原 Repository。三类失败均可检查并取回实际拥有者。

物理数据库候选库只接受同一 acquisition 的 Guard。释放锁再按相同路径或相同锁文件 FID 重新 acquire 不构成连续交接。DB/WAL/SHM/JSON 的完整物理安装及失败回滚是后续独立的安装协议；这些 Rust 接口尚未映射为 CLI 的物理恢复命令。
