# 系统控制接口

Windows 系统媒体会话、全局热键和电源通知通过同一个后端 actor 执行播放命令。`system_control` 保存在 `player-state.json`；旧配置缺少该字段时使用默认设置，`enabled` 和 `global_hotkeys_enabled` 默认关闭。

## 命令与查询

| JSON 接口 | 行为 |
| --- | --- |
| `{"query":"system_control_settings"}` | 读取持久设置 |
| `{"query":"system_control_state"}` | 读取实际原生拥有者、配置阶段、退休和存储恢复状态 |
| `set_system_control_settings` | 验证设置，等待原生配置确认后提交持久配置 |
| `retry_system_control` | 重试尚未完成的原生退休、配置回滚或存储恢复 |

关闭所有系统控制的完整命令：

```json
{
  "request_id": "system-disable-1",
  "origin": "cli",
  "command": "set_system_control_settings",
  "settings": {
    "enabled": false,
    "media_session_enabled": false,
    "global_hotkeys_enabled": false,
    "power_blocker_enabled": false,
    "hotkeys": []
  }
}
```

`enabled` 为总开关。`media_session_enabled`、`global_hotkeys_enabled` 和 `power_blocker_enabled` 分别控制媒体会话、热键和播放期间的电源请求。Windows 以外的平台可保存关闭的设置，启用原生服务返回 `Unavailable`。

`hotkeys` 最多 32 项，每项包含 `chord` 和 `action`。`chord` 使用 `control/alt/shift/windows` 布尔值和 `virtual_key`（1–254）。重复组合键、无修饰键的普通按键、与媒体会话重复占用的媒体键以及把电源通知绑定为热键均被拒绝。

## 接受、提交与恢复

已有原生资源时，配置和关闭操作可返回 `Accepted`，调用方按相同 `operation_id` 等待 `OperationFinished`。设置查询只返回已经提交的偏好；`configuration` 投影原生配置的实际阶段和确认结果。

原生配置确认之后持久提交失败，后端暂停系统输入并恢复旧偏好。恢复未完成期间，`store_recovery_pending` 为 true，`store_recovery_operation` 保留原操作 ID；其他配置文件写入返回 `Storage`。修复实际存储故障后执行 `retry_system_control`，恢复结束后原操作仍以其原始存储错误结束。

系统输入同时绑定拥有者、配置 revision、取消状态以及适用的播放 generation。旧配置、过期媒体或已取消的排队输入不会修改当前播放。绝对和相对定位要求实际已经呈现的可定位媒体；暂停、静音和供给不足采用后端的真实音频时钟。

电源挂起暂停播放；自动或用户恢复通知记录状态，保持暂停。关机先关闭输入，再清除媒体会话、热键、电源请求和图片引用，观察期限结束后仍保留尚未物理退出的拥有者。

## 验证范围

公共后端测试覆盖旧配置默认值、保存与重启、无效热键的零修改、实际存储故障与恢复。内联测试覆盖配置确认与回滚、排队输入取消、图片证明工作线程及原生图片引用的实际寿命。原生桌面媒体会话、全局热键、系统电源通知和协议注册分别执行桌面验收；这些结果不由配置或回滚测试替代。
