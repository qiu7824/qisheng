# 定时停止

定时器由后端 actor 使用单调时钟运行，暂停、跳转和手动选曲期间继续计时。等待选项和上次时长持久化；活动倒计时仅属于当前进程，配置备份和重启不会恢复过期动作。计时范围为大于零且不超过 2,147,483.647 秒，非法值不会替换既有定时器。

```json
{"command":"start_timeout_stop","seconds":1800,"wait_for_track_end":true}
```

`wait_for_track_end` 缺省使用已保存的选项，初值为 `true`。到期后保留已经呈现媒体帧的当前曲目，拦截自动下一曲、单曲循环、未呈现的预加载提升和失败恢复；当前曲目完成后保留结束状态。尚未开始呈现的加载暂停，晚到解析不能自动开始播放。设为 `false` 时，到期暂停当前播放并保存进度。两种模式均保留曲目、队列和播放位置，明确的播放、恢复、切换命令解除到期锁存。

```json
{"command":"cancel_timeout_stop"}
{"query":"timeout_stop"}
{"query":"timeout_stop_preferences"}
{"command":"set_timeout_stop_preferences","settings":{"wait_for_track_end":false,"last_seconds":1800}}
```

查询返回配置序号、`idle`／`armed`／`waiting_for_track_end`／`elapsed` 状态、初始秒数、剩余毫秒和自动播放锁存。剩余时间来自实际单调截止时间；状态变化发出 `timeout_stop_changed` 事件。取消或重新设置清除到期锁存，不主动开始播放。

参考行为固定为 LX Music Desktop v2.12.6 的 `src/renderer/core/player/timeoutStop.ts` 和 `usePlayEvent.ts`。原始 `player.waitPlayEndStop` 与 `player.waitPlayEndStopTime` 分别映射等待选项和整数分钟输入；兼容范围为 1–1440 分钟。项目备份保留任意合法秒数，LX 设置导出仅投影可表达的整数分钟，其余输出空输入值。活动倒计时不进入两种备份。后端轮询周期为 50 毫秒，繁忙操作的执行时间仍影响实际控制延迟。
