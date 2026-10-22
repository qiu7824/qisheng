# 音效与脉冲响应接口

音效设置由独立后端保存，音频输出报告实际呈现的银行序号。调用方通过命令修改设置，通过查询获取资源库存、操作结果和实际音频状态。

## 命令

| command | 字段 | 行为 |
| --- | --- | --- |
| `set_effects` | `settings` | 保存并准备卷积及空间声设置 |
| `apply_bundled_impulse_preset` | `id` | 应用内置卷积预设的原版直达与发送增益，保留当前空间声设置 |
| `reset_effects` | 无 | 恢复默认音效设置 |
| `import_impulse_resource` | `path`, `file_read_approved`, 可选 `timeout_ms` | 从明确批准的 WAV/FLAC 文件导入独立持久资源 |
| `recover_impulse_import` | `token`, 可选 `timeout_ms` | 根据持久库存恢复已完成的实际导入对象 |
| `recover_impulse_namespace` | `selection`, 可选 `timeout_ms` | 根据当前持久创建记录与本次实际选择恢复未完成的资源命名空间 |
| `abort_impulse_import` | `token`, 可选 `selection` | 回收失败或取消后保留的实际分配；冷恢复需明确文件选择及读写批准 |
| `retire_impulse_resource` | `resource_id` | 回收未被设置选中的实际资源 |
| `recover_impulse_retirement` | `token` | 继续已有实际退休操作 |
| `cancel_effects_operation` | `operation_id` | 取消有界准备操作 |

所有命令采用 `CommandEnvelope`，包含非空 `request_id`，支持 `origin` 与 `expected_revision`。异步操作返回 `Accepted` 后，通过 `effects_operations` 或 `OperationFinished` 获取终态。资源准备完成不代表音频已经呈现。

`abort_impulse_import.selection` 包含 `path`、`file_read_approved` 和 `file_write_approved`。同一运行实例优先使用仍持有的实际创建句柄；重启后的回收校验本次选中文件的实际身份、完整当前内容和变化戳。缺少已登记实际文件身份的分配保持拒绝。回收终态须同时满足持久删除确认与实际工作线程退出，随后压缩库存并释放额度。

`recover_impulse_namespace.selection` 包含 `directory_path`、`directory_read_approved`、`directory_write_approved` 及可选 `owner`。目录必须为当前 profile 的实际 `.lx-impulse-vault`，读写批准均须为 `true`。未知目录身份的冷恢复需要本次实际目录选择；目录中存在未知身份的所有者文件时，`owner` 包含该文件的 `path`、`file_read_approved` 与 `file_write_approved`。后端核对原生文件身份、完整当前内容和变化戳，再取得同一对象的写句柄。外来内容或其他目录对象导致恢复拒绝。持久创建记录仅描述当前未完成操作，不能作为目录或文件访问授权；调用方不能提交另一份创建记录。终态等待库存持久确认与实际工作线程退出。

```json
{
  "request_id": "effect-import-1",
  "command": "import_impulse_resource",
  "path": "C:\\Audio\\room.wav",
  "file_read_approved": true,
  "timeout_ms": 30000
}
```

导入结果中的 `resource_id` 采用 `{"kind":"imported","id":"32位十六进制标识"}`。该标识仅用于选择已登记资源，不提供路径访问权限。

以下命令设置直达与发送增益；选择卷积响应时，将 `impulse` 替换为库存返回的完整 `resource_id` 对象。

```json
{
  "request_id": "effect-select-1",
  "command": "set_effects",
  "settings": {
    "convolution": {
      "impulse": null,
      "main_gain": 1.0,
      "send_gain": 0.25
    },
    "spatial": {"enabled": false}
  }
}
```

`main_gain` 与 `send_gain` 必须有限且在 0–5 范围内。`timeout_ms` 在 1–120000 范围内。省略设置字段时使用领域默认值；未知设置和命令字段被拒绝。

## 查询

| query | 返回内容 |
| --- | --- |
| `effects_settings` | 持久设置 |
| `effects_state` | `persisted`、实际音频 `audio`、资源准备状态及错误 |
| `bundled_impulse_presets` | 13 个内置预设的资源标识、增益、SHA-256、采样率、声道与帧数 |
| `impulse_resources` | 命名空间、资源、未完成导入与退休库存 |
| `effects_operations` | 有界操作记录与终态 |

查询结果采用 `{"kind":"查询名","data":结果}`。`effects_state.audio.requested_sequence` 表示请求序号；`applied_sequence` 仅随实际音频呈现推进。保存成功、资源解码完成或空闲状态都不能作为音频生效的依据。

## 资源与权限

编码输入最大 8 MiB，支持 WAV/FLAC。解码响应支持 1、2、4 声道且最长 4 秒。每个 profile 最多保留 32 个已登记响应；私有命名空间限制 64 MiB 和 64 个目录对象，未完成分配仍占用额度。

持久资源存放于 profile 的 `.lx-impulse-vault`。重新打开时校验当前 profile、目录与所有者文件的实际身份，并核对响应文件身份、单链接、头部和摘要。原选择文件可在导入成功后移除。外来对象、身份变化和未完成分配通过库存报告，不能仅按路径重新认领或删除。

内置预设包括 `telephone`、`s2_r4_bd`、`bright_hall`、`cinema_diningroom`、`dining_living_true_stereo`、`living_bedroom_leveled`、`spreader50_65ms`、`s3_r1_bd`、`matrix_1`、`matrix_2`、`cardiod_35_10_spread`、`tim_omni_35_10_magnetic` 与 `feedback_spring`。编码数据嵌入程序，选中后才校验摘要并按预算解码；不创建私有导入目录或文件授权。`set_effects` 也接受查询返回的 `{"kind":"bundled","id":"预设标识"}`，可独立调整增益。压缩器沿用固定兼容默认参数；固定上游没有单独的压缩器偏好或预设入口。原版参数与资源定义见[固定播放器实现](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/renderer/plugins/player/index.ts)，许可文本随程序保留。

备份恢复保留当前音效资源权限与未完成操作库存。备份设置及序列化资源标识不能建立新的文件访问或删除权限。

## CLI

```text
lx-native-cli --audio memory --data-dir PROFILE import-impulse PATH
lx-native-cli --audio memory --data-dir PROFILE effects
lx-native-cli --audio memory --data-dir PROFILE effects-presets
lx-native-cli --audio memory --data-dir PROFILE apply-effect-preset bright_hall
lx-native-cli --audio memory --data-dir PROFILE impulse-resources
```

`--json-lines` 支持同一连接内提交、查询与取消。单次音效命令先等待已保存资源的启动准备结束，再提交修改并等待异步终态；正常输入 EOF 执行显式关机并等待所属工作线程退出。
