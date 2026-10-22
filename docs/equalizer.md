# 十段均衡器接口

均衡器在 Device 和 Memory 共用的 PCM 渲染内核中处理音频。解码线程先执行倍速保调或变速变调，并为 PCM 帧附加原始媒体坐标；输出侧处理顺序为采样率与声道转换、十段均衡器、独立 pitch、保存的音量与静音、最终限幅。各声道分别维护滤波历史，最多支持 8 个输出声道。

## 参数与预设

`EqualizerSettings` 包含固定长度的 `gains_db: [f64;10]`，顺序对应 31、62、125、250、500、1000、2000、4000、8000、16000 Hz。每项单位为 dB，接受 −15 至 +15 的有限数值，支持小数。每段为 peaking 滤波器，Q=1.4；默认十项均为 0 dB，平直状态保持输入样本。

JSON 必须包含完整十项数组。错误长度、错误类型、未知设置字段、非有限数值和越界增益均被拒绝。设置不包含独立启用开关或前级增益；`reset_equalizer` 将全部增益恢复为零。

`EqualizerPreset` 包含 `flat` 及固定 LX v2.12.6 的九个预设：

| 预设 | 按频段顺序排列的增益，dB |
| --- | --- |
| flat | [0,0,0,0,0,0,0,0,0,0] |
| pop | [6,5,−3,−2,5,4,−4,−3,6,4] |
| dance | [4,3,−4,−6,0,0,3,4,4,5] |
| rock | [7,6,2,1,−3,−4,2,1,4,5] |
| classical | [6,7,1,2,−1,1,−4,−6,−7,−8] |
| vocal | [−5,−6,−4,−3,3,4,5,4,−3,−3] |
| slow | [5,4,2,0,−2,0,3,6,7,8] |
| electronic | [6,5,0,−5,−4,0,6,8,8,7] |
| subwoofer | [8,7,5,4,0,0,0,0,0,0] |
| soft | [−5,−5,−4,−4,3,2,4,4,0,0] |

预设查询返回十个 `EqualizerPresetInfo`，字段为 `preset`、固定小写 `name` 和 `settings`。选择预设保存其完整增益数组；后续修改单段不会隐式重新套用预设。

均衡器设置保存为 `player-state.json` 中的 `data.equalizer`：

```json
{"gains_db":[0,0,0,0,0,6,0,0,0,0]}
```

存储版本保持 1。旧记录缺少 `equalizer` 时恢复平直设置，读取不会改写文件。校验与持久化失败保持原设置；均衡器不改变保存的音量、静音、设备目标或播放地址许可。

## 输出采样率与频响

滤波系数按实际输出采样率以 f64 准备。十段串联，相邻频段的响应会叠加，最终某一频率的增益不一定等于对应单段的设置值。系数与历史均使用 f64，最终按输出样本格式转换。

频段达到或超过输出 Nyquist 频率时采用单位响应，保存的增益保持原值。例如 Memory 的 22050 Hz 输出下，16000 Hz 段不参与处理；切换到能够处理该频段的采样率后按保存值准备系数。零增益段也采用单位响应。设备切换重新准备实际采样率的计划，不使用旧采样率系数处理新端点。

连续有效 PCM 上的设置变化使用两组固定滤波器状态线性混合，过渡长度为 `ceil(sample_rate/50)` 个输出帧，约 20 ms；22050、44100、48000 Hz 分别为 441、882、960 帧。这是参数过渡长度，不能作为整条音频路径的延迟值。

过渡过程中收到新设置时仅保留最新待应用计划，当前过渡结束后接续处理。两组滤波器处理同一个已读取的媒体帧，不增加源帧消费。暂停、seek、媒体替换、缺帧及 EOF 边界清除 EQ 历史并采用最新计划；EQ 自身不追加 EOF 尾音。后级 pitch 保留尚未输出的有效样本及标签，在正常 EOF 后完成有限排空，输出结束须等待这些标签送出和排空完成。

暂停与设备切换冻结尚未输出的 pitch 帧；已经从延迟队列取出但未被接受的帧进入单个固定暂存槽，恢复后优先回放。seek、媒体 token 或 epoch 改变、源错误及关闭使旧帧失效并清除。静音在最终输出阶段置零，仍可送出有效标签并推进媒体时钟；启动补零及没有有效标签的排空帧不推进时钟。

## 目标与渲染确认

`EqualizerSnapshot` 由保存的目标与渲染回调确认状态组成：

| 字段 | 含义 |
| --- | --- |
| `desired` | 最新目标增益数组。 |
| `applied` | 渲染内核已确认接入的目标计划。过渡期间输出仍混合旧、新响应。 |
| `requested_sequence` | 本次运行的设置请求序号。 |
| `applied_sequence` | 渲染内核确认的计划序号。 |
| `sample_rate` | 最近一次被渲染内核确认的处理采样率；尚无确认时为 `null`。 |
| `active_bands` | 已确认计划中实际参与处理的十段标记。 |
| `transition_remaining_frames` | 当前混合过渡剩余的输出帧数；零表示该过渡已结束。 |
| `clipped_samples` | 当前输出实例中最终增益之后、限幅之前越界的样本累计数。 |
| `non_finite_samples` | 当前输出实例中检测并置零的非有限输出样本累计数。 |
| `peak_before_clamp` | 当前输出实例中最终增益之后、限幅之前观测到的最大有限绝对峰值。 |

提交设置只推进目标和请求序号；`committed` 回执不表示已经渲染或已经完成过渡。渲染确认后的 `applied_sequence` 与 `sample_rate`、增益数组、频段标记及过渡余量来自同一计划。设置恢复或提交可以使 `requested_sequence` 大于 `applied_sequence`；序号只用于当前会话，不持久化或跨重启比较。

EQ 的确认描述已接入渲染内核的滤波目标。后级 pitch 队列可能包含此前已通过 EQ 的样本，最终输出的频响变化还包含该队列的处理延迟。倍速与独立 pitch 的实际设置、过渡和延迟分别由 `PlaybackProcessingSnapshot` 表示，见[播放处理接口](playback-processing.md)。

尚未创建输出时，仅缓存目标和待应用设置；均衡器命令本身不创建 Device 端点或 Memory 渲染线程。首次允许播放的媒体完成 PCM 准备后应用目标；Device 的明确设备选择或刷新也可以启动其输出拥有者，详见[设备接口](audio-output.md)。Memory 格式预览不等于均衡器渲染确认，均衡器的 `sample_rate` 可以仍为 `null`。

`sample_rate` 描述最近确认的 DSP 计划。设备是否正在输出、是否处于切换或失效状态，由 `audio_output.state` 表示。设备交还保持同一输出实例的统计；输出实例重建会重新累计。静音后的零增益不会增加峰值，但已经观测到的历史最大值保留。

## 后端与 CLI

命令通过统一后端和 `lx-native-cli --audio memory --json-lines` 提交；使用物理端点时选择 `--audio device`。每行一个 JSON 对象：

```json
{"request_id":"eq-gains-1","command":"set_equalizer","settings":{"gains_db":[0,0,0,0,0,6,0,0,0,0]}}
{"request_id":"eq-rock-1","command":"set_equalizer_preset","preset":"rock"}
{"request_id":"eq-flat-1","command":"reset_equalizer"}
```

三种设置命令返回 `committed`。可选 `origin` 和 `expected_revision` 沿用通用命令封装。相同请求 ID 的相同输入在会话内返回原结果；不同输入复用该 ID 返回冲突。失败后的重试使用新的请求 ID。

查询：

```json
{"query":"equalizer_settings"}
{"query":"equalizer_presets"}
{"query":"equalizer_state"}
```

`equalizer_settings` 返回保存的目标，示例响应为：

```json
{"ok":true,"result":{"kind":"equalizer_settings","data":{"gains_db":[0,0,0,0,0,6,0,0,0,0]}}}
```

`equalizer_presets` 的 `result.kind` 为 `equalizer_presets`，`result.data` 为十项预设数组。数组中单个 rock 项的结构：

```json
{"preset":"rock","name":"rock","settings":{"gains_db":[7,6,2,1,-3,-4,2,1,4,5]}}
```

目标已经提交、尚未创建输出时，`equalizer_state` 可以返回：

```json
{
  "ok": true,
  "result": {
    "kind": "equalizer_state",
    "data": {
      "desired": {"gains_db": [0,0,0,0,0,6,0,0,0,0]},
      "applied": {"gains_db": [0,0,0,0,0,0,0,0,0,0]},
      "requested_sequence": 2,
      "applied_sequence": 0,
      "sample_rate": null,
      "active_bands": [false,false,false,false,false,false,false,false,false,false],
      "transition_remaining_frames": 0,
      "clipped_samples": 0,
      "non_finite_samples": 0,
      "peak_before_clamp": 0
    }
  }
}
```

普通状态与 `snapshot_cursor` 的完整快照通过 `equalizer` 字段提供相同结构。目标、请求/确认序号、处理采样率、频段标记或过渡活动状态变化时发布 `equalizer_changed`。事件包装沿用 `EventEnvelope`：`origin="audio"`、`operation_id=null`，`payload` 中包含 `event="equalizer_changed"` 和完整 `state`。

```json
{"query":"snapshot_cursor"}
{"query":"events","cursor":12}
```

先取得快照及配套 `result.cursor`，再在同一运行实例中查询该游标之后的事件；上面的 12 表示取得的游标值。事件不按每个 PCM 帧发布，也不因峰值或累计计数单独持续发布。过渡可以在两次状态观察之间完成；统计和当前剩余帧数通过 `equalizer_state` 查询。游标过期时重新取得 `snapshot_cursor`。

## 资源与验证边界

系数、稳定性检查和计划准备在渲染回调之外完成。回调使用固定的两组状态、单个最新待应用计划和有界原子邮箱，读取邮箱的尝试次数固定；各组最多执行 8 声道、10 段滤波。数据渲染和 `RenderKernel` 析构不分配或释放堆内存，不持有设置互斥锁。错误载荷与设备关闭的生命周期边界见[设备接口](audio-output.md)。

小于 `1e-300` 的 f64 历史值置零。遇到非有限输出时清除对应声道状态，输出零并累计计数。最终样本限制在 [−1,1]，超出范围的样本累计到 `clipped_samples`。当前链未接入压缩器、自动衰减、卷积、空间声或分析器；较高增益可以增加峰值和限幅次数。

数值与无界面验证入口包括 `src/audio/dsp.rs`、`src/audio/output.rs`、`src/audio/output/device_owner.rs` 中的单元测试，以及 `tests/store_equalizer.rs`、`tests/backend_equalizer.rs` 和 `tests/fixtures/equalizer_reference.json`。独立参考覆盖 22050、44100、48000 Hz 的系数、稳定性、脉冲及频响；集成验证观察真实 Memory PCM 和媒体时钟。

固定存储与运算上限不构成目标设备 CPU、驱动延迟或硬件发声的性能验收结果。真实设备上的持续负载、端点切换与主观听音分别验收；输出采样率转换仍沿用现有转换器，卷积、空间声、压缩器、分析器及系统媒体功能继续按 M06 推进。
