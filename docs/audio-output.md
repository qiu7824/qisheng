# 音频输出设备接口

音频输出通过统一后端提供设备清单、目标选择、实际端点状态、静音和设备变化暂停策略。Device 模式使用 CPAL 输出端点；Memory 模式执行真实解码和 PCM 渲染。设备状态与播放媒体状态分别表示，设备切换不会创建新的播放曲目或清除音频缓存。

## 设置与持久化

`AudioPreferences` 包含三个字段：

| 字段 | 默认值 | 含义 |
| --- | --- | --- |
| `selection` | `{"type":"default"}` | 跟随系统默认输出设备，或指定稳定设备 ID。 |
| `muted` | `false` | 将最终输出增益置零，保持播放媒体时钟推进。 |
| `pause_on_device_change` | `false` | 实际输出端点因设备拓扑变化而改变时暂停播放。 |

指定设备采用 `{"type":"device","id":"稳定设备ID"}`。ID 使用 CPAL `DeviceId` 的文本表示，包含音频宿主和设备标识；显示名称及枚举位置不参与身份判断。客户端应从 `audio_devices` 查询中取得 ID，并原样提交。

设置保存在 `player-state.json` 的 `data.audio_preferences`。存储版本继续为 1；旧记录缺少该字段时使用默认值，读取不会改写文件。设置对象缺少新字段时也使用对应默认值。设备清单、活动端点、输出模式和切换序号属于运行状态，不写入偏好设置。

```json
{
  "version": 1,
  "data": {
    "sources": [],
    "selected": null,
    "local_files": [],
    "volume": 0.7,
    "audio_preferences": {
      "selection": {"type": "default"},
      "muted": false,
      "pause_on_device_change": false
    }
  }
}
```

音量独立保存在 `data.volume`，默认 0.7。静音不修改这个值；静音期间仍可调整保存的音量，取消静音后按该音量输出。

设置命令先校验并持久化，再交给音频引擎应用。校验或存储失败时保持原设置和运行偏好；损坏的主文件或中断替换后留下的损坏备份不会被默认设置覆盖。持久化回执表示偏好提交完成，端点是否已打开须读取实际输出状态。

## 校验与容量

显式选择的 ID 必须非空、UTF-8 长度不超过 1024 字节、不含控制字符，能够通过 CPAL `DeviceId` 纯文本解析，并且首个冒号后有非空设备标识。解析不枚举或打开设备。格式错误、未知宿主和空标识返回 `invalid_data`；格式合法但当前不存在的设备可以保存为目标。

设备清单最多 128 项，ID 唯一，最多一个系统默认设备；可用默认端点优先保留。名称最多 512 个 UTF-8 字节；设备名称在字符边界截断并移除控制字符，空名称显示为 `Audio output`。设备 ID 不截断。已经保存的显式目标未出现在有限清单中时，拥有者还会按 ID 直接查询端点；清单截断本身不触发回退。实际输出采样率必须为正数，声道数为 1–8。

后端命令通道和事件日志保持有界。设备偏好通过单个待应用槽合并，设备刷新也合并为待处理标记，避免设备 I/O 占用播放命令队列。

## 目标与实际状态

`AudioOutputSnapshot` 的结构为：

| 字段 | 内容 |
| --- | --- |
| `devices` | `AudioDeviceInfo[]`，每项包含 `id`、`name`、`is_default`。 |
| `preferences` | 当前音频输出偏好。 |
| `state` | 目标、活动端点、实际格式、切换状态和固定错误类别。 |

`state` 字段如下：

| 字段 | 含义 |
| --- | --- |
| `desired` | 保存的默认设备策略或显式设备目标。 |
| `active_device_id` / `active_device_name` | 已确认接管 PCM 的端点；没有活动端点时为 `null`。 |
| `format` | Device 已确认输出的 `sample_rate` 和 `channels`；初始化前为 `null`。Memory 预览并使用固定渲染格式。 |
| `status` | `initializing`、`ready`、`switching`、`unavailable`、`closed` 或 `not_applicable`。 |
| `switch_sequence` | 输出切换尝试的运行序号；与仓库 revision、媒体 generation、seek 序号分别管理。 |
| `fallback` | 显式目标缺失时已选择系统默认端点作为回退。 |
| `last_error` | 固定错误类别或 `null`。 |
| `mode` | `device` 或 `memory`。 |

`ready` 表示当前有可用的活动端点。新的目标打开失败时，原活动端点可以继续保持 `ready`，同时记录 `last_error`；因此应同时检查目标、活动 ID 和错误。活动 ID 与实际格式在新回调确认取得渲染状态后发布。

显式目标在系统中不可用时，拥有者尝试系统默认端点，保留原 `desired`，并在成功回退后设置 `fallback=true`、`last_error="device_not_available"`。目标重新可用时自动尝试返回该端点。目标、默认回退及原活动端点均不可用时保留媒体 PCM，输出状态为 `unavailable`。目标仍存在但配置、建流或启动失败时保留可用的原端点，不将该失败当作目标缺失回退。

固定错误类别为 `device_not_available`、`stream_invalidated`、`buffer_underrun`、`backend_specific`、`inventory_failed`、`configuration_unavailable`、`stream_build_failed`、`stream_start_failed`、`invalid_selection` 和 `worker_unavailable`。错误回调只提交固定类别与流 generation；旧流的迟到错误不能修改新流状态。

## 输出初始化

输出采用延迟初始化。启动时恢复设置、读取快照及单独修改静音或暂停策略，只更新缓存和待应用偏好，不创建输出端点或渲染线程。

Device 初始化前显示 `status="initializing"`、空设备清单、空活动端点及 `format=null`。明确执行 `set_audio_output_device` 会安排一次设备刷新；明确执行 `refresh_audio_devices` 也会启动设备拥有者。首次允许播放的媒体 URI 完成解码准备后，会按保存的目标创建输出。开始初始化后，设备枚举、打开和拓扑检查均由拥有者执行。

Memory 的设置选择与设备刷新只更新预览状态；首次允许播放的媒体完成 PCM 解码准备后才创建渲染线程。首次远程播放的地址尚未批准时，不创建 Memory 渲染线程，渲染批次计数保持零。

## 切换与暂停策略

一个受监督的设备拥有者执行枚举、配置查询、打开、启动、关闭和重连。定期拓扑检查间隔为 1 秒；显式刷新唤醒同一拥有者。命令和查询读取缓存状态，不在后端 actor 上直接执行设备 I/O。

新端点准备期间原端点继续输出。新的 pitch 格式、FFT 计划、固定缓冲与重放容量在隔离旧回调之前准备并核对共享 DSP 配额；准备失败时保留原活动端点及媒体。候选端点准备完成后，拥有者隔离旧回调，将唯一渲染状态交还并转移到新回调，按新实际格式重定向转换器和 DSP。

PCM 源、已呈现的媒体位置、暂停状态、增益、静音、媒体 token 和 seek epoch 保持连续；不会重新解析播放地址、重启解码或重置缓存。尚未输出的 pitch 样本、原始坐标标签及单个暂存呈现帧保持顺序；采样率改变时由拥有者在回调之外转换并重放，准备好的输出优先送出，再读取新 PCM。候选失败或设备暂时不可用期间，已经停放的 PCM 不被消费。

暂停及设备切换冻结未输出的 pitch 内容。暂停在一个呈现帧已经取出后到达时，该帧保持在固定暂存槽，恢复后优先回放；seek、媒体 token 或 epoch 改变、源错误及关闭清除失效内容。媒体时钟仅在有效原始坐标标签被接受后推进，静音保留该标签语义；解码预取、pitch 启动补零和没有有效标签的排空帧不推进时钟。

输出处理顺序为采样率与声道转换、EQ、独立 pitch、音量与静音、最终限幅；倍速保调或变速变调在解码线程完成。非单位 pitch 首次启用后的常规延迟为 4096 个输出帧，正常 EOF 采用有界零输入排空，待有效内容与暂存帧全部送出后才报告输出结束。处理快照的采样率与延迟使用实际输出格式；转交 PCM 时钟的延迟换算回源 PCM 帧单位，原始媒体坐标的分母保持源 PCM 采样率，见[播放处理接口](playback-processing.md)。

`RenderKernel` 的数据渲染与析构使用预分配的 PCM、控制和交还队列，不在回调中分配或释放堆内存。PCM 源及队列的最终回收由拥有者执行。固定错误上报使用原子标记；CPAL `BackendSpecific` 错误携带的自有字符串会随错误参数析构，该错误载荷的生命周期不属于渲染内核的零堆操作保证。

暂停策略区分用户选择和拓扑变化：

- 用户明确选择另一个端点时，切换保持原播放或暂停状态。
- 拓扑变化使实际活动 ID 改变，且存在活动媒体、原状态正在播放、策略已启用时，切换过程暂停播放。包括默认端点变化、显式目标失效后的回退及目标恢复后的自动返回。
- 同一 ID 的重连、无关端点增删、名称变化、没有媒体时的初始化和重复刷新不触发该暂停策略。
- 已经暂停的播放保持暂停；策略触发暂停后由常规 `resume` 或 `toggle` 恢复。

## Memory 模式

CLI 默认使用 Memory，可通过 `--audio memory` 明确指定。它不枚举或打开物理设备；`devices=[]`，活动设备 ID 和名称为 `null`，`mode="memory"`。正常运行的输出状态为 `not_applicable`；引擎初始化或工作线程异常仍可报告 `unavailable` 与 `worker_unavailable`。渲染线程创建前，`format={"sample_rate":22050,"channels":1}` 表示固定格式预览；PCM 准备完成后的实际渲染使用该格式。该预览不表示已经创建渲染线程或活动音频设备。

Memory 仍保存并展示显式 `desired`、静音和暂停策略。刷新设备清单不会切换到 Device 模式或打断 PCM。静音输出零增益，同时继续消费真实解码的媒体帧；暂停停止媒体帧消费。

Memory 每批渲染 441 个输出帧，以连续的 20 ms 截止时间调度并保留正常定时器晚唤醒的时间差。累计落后超过 100 ms 时重建调度基准并记录超期；内核执行成本与整批成本分别记录。该调度和诊断不构成物理设备的实时性能验收。

## 命令与查询

JSON 命令通过 `lx-native-cli --audio memory --json-lines` 或 `--audio device --json-lines` 逐行提交。每行一个 JSON 对象。成功响应包装为 `{"ok":true,"result":...}`；命令的 `result` 包含 `receipt` 与 `snapshot`。JSON-lines 错误响应为 `{"ok":false,"error":"错误信息"}`，Rust 后端接口另提供结构化 `BackendError`。

以下命令使用不同的请求 ID：

```json
{"request_id":"output-default-1","command":"set_audio_output_device","selection":{"type":"default"}}
{"request_id":"output-mute-1","command":"set_mute","muted":true}
{"request_id":"output-pause-policy-1","command":"set_pause_on_device_change","enabled":true}
{"request_id":"output-refresh-1","command":"refresh_audio_devices"}
```

显式选择的结构示例；运行时以清单返回的完整 ID 替换示例值：

```json
{"request_id":"output-device-1","command":"set_audio_output_device","selection":{"type":"device","id":"wasapi:example-output-id"}}
```

`set_audio_output_device`、`set_mute` 和 `set_pause_on_device_change` 返回 `committed` 回执；`refresh_audio_devices` 返回 `applied`，表示已安排刷新。回执不等待设备建流完成。可选 `origin` 默认 `local`，可选 `expected_revision` 用于仓库版本冲突检查。

同一后端会话的有界请求记录中，相同 `request_id` 与完全相同输入返回原结果；相同 ID 配合不同输入返回 `conflict`。命令失败后的相同输入也保留原失败结果，重试应使用新的请求 ID。

查询请求：

```json
{"query":"audio_settings"}
{"query":"audio_output_state"}
{"query":"audio_devices"}
```

`audio_settings` 的响应示例：

```json
{"ok":true,"result":{"kind":"audio_settings","data":{"selection":{"type":"default"},"muted":true,"pause_on_device_change":true}}}
```

Memory 下的输出状态响应：

```json
{
  "ok": true,
  "result": {
    "kind": "audio_output_state",
    "data": {
      "desired": {"type": "default"},
      "active_device_id": null,
      "active_device_name": null,
      "format": {"sample_rate": 22050, "channels": 1},
      "status": "not_applicable",
      "switch_sequence": 0,
      "fallback": false,
      "last_error": null,
      "mode": "memory"
    }
  }
}
```

Memory 清单响应为 `{"ok":true,"result":{"kind":"audio_devices","data":[]}}`。普通状态查询 `{"query":"status"}` 的 `result.audio_output` 包含完整 `AudioOutputSnapshot`。

## 事件与游标

输出快照发生变化时发布 `audio_output_changed`。事件以 `origin="audio"`、`operation_id=null` 表示设备状态观察，与命令回执分别处理；未改变快照的 Memory 刷新不产生输出变化事件。

事件日志中单个 `EventEnvelope` 的结构示例：

```json
{
  "event_id": 12,
  "repository_revision": 3,
  "operation_id": null,
  "origin": "audio",
  "payload": {
    "event": "audio_output_changed",
    "output": {
      "devices": [],
      "state": {
        "desired": {"type": "default"},
        "active_device_id": null,
        "active_device_name": null,
        "format": {"sample_rate": 22050, "channels": 1},
        "status": "not_applicable",
        "switch_sequence": 0,
        "fallback": false,
        "last_error": null,
        "mode": "memory"
      },
      "preferences": {
        "selection": {"type": "default"},
        "muted": true,
        "pause_on_device_change": true
      }
    }
  }
}
```

使用一致的快照与游标建立订阅起点：

```json
{"query":"snapshot_cursor"}
```

成功响应的 `result.snapshot` 是完整 `BackendSnapshot`，`result.snapshot.audio_output` 具有上述 `output` 结构；`result.cursor` 是与该快照配套的事件起点。取得游标后在同一运行实例中查询后续事件，例如游标为 12 时：

```json
{"query":"events","cursor":12}
```

成功响应的 `result` 为事件数组，仅包含 `event_id>12` 的事件。处理后将游标推进到最后一个已处理事件的 ID。日志最多保留 256 个事件；Rust 接口返回 `cursor_expired` 时重新获取 `snapshot_cursor`。游标不跨进程或重启使用。

## 关闭与验证边界

`BackendHandle::shutdown(timeout)` 和音频关闭接口按调用者给定的期限等待实际拥有者 join。音频线程退出前会继续管理尚未关闭的输出端点及回调交还；等待多个音频资源时使用剩余预算。关闭超时返回错误，仍在枚举、打开、关闭或等待回调析构的线程保留监督归属，后续关闭调用可再次等待。只有注册拥有者完成实际 join 后才报告对应关闭成功。

CPAL WASAPI 的建流超时和私有流线程退出由其实现管理。上述期限约束注册的应用拥有者及 PCM 回调交还，不代表能够强制终止驱动或保证 CPAL 所有内部私有线程均已 join。

无界面验证入口包含 `tests/store_audio_preferences.rs`、`tests/backend_audio_output.rs`、`tests/audio_output.rs`，以及 `src/audio/output/device_owner.rs` 和 `src/audio/worker_owner.rs` 的单元测试。测试覆盖旧设置读取与重启、失败提交保护、真实 Memory PCM 的静音与时钟、端点切换状态交还、回退与恢复、迟到回调隔离、拓扑暂停和关闭超时后的监督 join。

物理测试 `physical_two_endpoint_switch_preserves_the_consumed_pcm_media_clock` 默认忽略，要求两个具有不同稳定 ID 的可用 CPAL 输出端点，执行 A→B→A 切换并检查媒体位置与缓存连续性。Memory 和受控端点测试不表示物理扬声器验收通过。已接入倍速保调、变速变调、EQ 与独立 pitch；卷积、空间声、压缩器、分析器及系统媒体集成仍属于 M06 后续工作，硬件持续负载、驱动延迟和声学验证分别验收。
