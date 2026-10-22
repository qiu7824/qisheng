# 倍速、保调与独立移调接口

播放处理由解码工作线程的变速处理和输出渲染内核的独立移调组成。音频顺序为解码、倍速处理、输出采样率与声道转换、十段均衡器、独立移调、保存的音量与静音、最终限幅。Device 和 Memory 使用同一渲染路径，支持单声道至 8 声道；各声道的音频数据分别保存，WSOLA 使用共同的对齐位置维持声道间的时间关系。

## 设置与保存

`PlaybackProcessingSettings` 包含三个必填字段：

| 字段 | 类型与范围 | 默认值 | 含义 |
| --- | --- | --- | --- |
| `playback_rate` | f64，有限数值，0.5 至 2.0 | 1 | 相对于原始媒体的播放速度。 |
| `preserves_pitch` | bool | true | 倍速时保持原始音高。 |
| `pitch_ratio` | f64，有限数值，0.5 至 1.5 | 1 | 独立移调倍率，保持倍速已经确定的时长。 |

完整设置示例：

```json
{"playback_rate":1.5,"preserves_pitch":true,"pitch_ratio":0.75}
```

完整设置对象要求三个字段齐全。错误类型、未知设置字段、非有限数值和越界数值均被拒绝。单项命令更新对应字段，并保留另外两项。`reset_playback_processing` 恢复 1 倍速度、保调开启、独立移调倍率 1。

原始媒体时长为 D、原始稳定音调频率为 f 时，目标播放时长为 `D/playback_rate`。保调开启时，独立移调后的目标频率为 `f*pitch_ratio`；保调关闭时为 `f*playback_rate*pitch_ratio`。滤波窗口、启动与结束处理以及频谱网格影响短时输出，稳定音调的频率精度见后文。

设置保存在 `player-state.json` 的 `data.playback_processing` 中，存储版本保持 1。旧记录缺少该字段时恢复默认值，读取保持原文件。提交先完成校验与准入检查，再保存完整设置并发布运行目标；校验、准入或持久化失败保持原目标。设置不改变音量、静音、均衡器、输出设备目标、媒体地址许可或编码缓存身份。

倍速和移调命令只更新处理目标。媒体准备完成后按源格式建立处理器；设置本身不启动输出端点、创建 Memory 渲染线程或重新解析媒体地址。

## 原始媒体时钟与渲染确认

每个经过倍速处理的 PCM 帧携带固定标签：原始媒体结束位置 `end_frame`、Q0.32 小数 `fraction`、`processing_sequence`、生成该帧时的设置以及处理延迟。其原始位置为：

```text
original_frame = end_frame + fraction / 2^32
original_seconds = original_frame / source_sample_rate
```

标签使用生成该帧时的速度。队列中保留旧标签，后续设置不会重新缩放已经生成的音频。WSOLA 的原始时间标签沿连续的名义媒体游标推进；波形匹配在搜索范围内选择对齐位置，维持各声道一致的拼接。

采样率转换插值这些原始媒体区间。独立移调延迟音频时，同时延迟对应标签。最终输出帧通过媒体 token、PCM epoch、暂停、切换与关闭检查后，才提交原始媒体位置。预取和 FFT 启动补零保持播放位置；静音状态仍提交有效媒体标签。`consumed_frames` 统计实际 PCM 源读取量，原始媒体位置由渲染确认标签决定。

`PlaybackProcessingSnapshot` 字段如下：

| 字段 | 含义 |
| --- | --- |
| `desired` | 最新保存并发布的完整目标。 |
| `applied` | 最近有效输出标签确认的倍速设置与实际移调目标。过渡期间仍存在旧、新响应混合。 |
| `requested_sequence` | 当前运行实例的设置请求序号。 |
| `applied_sequence` | 已消费处理帧的倍速序号与移调序号中共同确认的进度，采用两者较小值。 |
| `latency_frames` | 已确认处理路径的延迟帧数，单位为实际输出采样率的帧。 |
| `transition_remaining_frames` | 当前移调混合或频谱过渡剩余的输出帧数。 |
| `sample_rate` | 该确认状态的实际输出采样率；尚无有效渲染确认时为 `null`。 |

保存提交、解码器就绪和有效输出标签确认分别发生。`committed` 回执表示设置已经保存；请求序号可以领先于确认序号。倍速和移调处于不同处理阶段，运行中可能先观察到某一阶段的更新；`applied` 保留实际确认的组合，序号报告共同进度。存在过渡余量时，当前音频仍混合旧、新状态。`playback.rate` 使用已确认的倍速。

序号只用于同一运行实例，重启后按恢复的目标重新建立请求与确认。旧队列、预加载帧和移调延迟帧继续携带自身标签，直到实际输出或被新的媒体 epoch 作废。

延迟按输出采样率换算：

```text
output_latency_frames =
    ceil(tempo_latency_frames * output_sample_rate / source_sample_rate)
    + pitch_latency_frames
latency_seconds = output_latency_frames / output_sample_rate
```

标签内部保留源采样率单位；公开状态使用输出采样率单位。帧数描述处理路径的缓冲状态，设备及驱动的额外传输延迟通过实机测量单独记录。

## 变速与移调方法

保调倍速采用固定存储的 WSOLA。合成 hop 为约 32 ms，周期 Hann 帧为约 64 ms，波形搜索范围为 ±32 ms。归一化相关使用全部声道共同选择一个对齐位置；粗搜索包含名义位置，最多 130 次相关计算，每次最多 256 个采样点；细搜索在支持的最高源采样率下最多 385 次计算，每次最多 512 个采样点。搜索候选与采样点之间检查取消和 seek 请求。

保调关闭时采用变速采样。32 tap、256 相位的窗化 sinc 表在解码工作线程准备，截止频率随下采样倍率调整，以抑制倍速升高时的混叠。速度为 1 时使用直接样本路径。处理器的源格式上限为 384000 Hz、8 声道，实际媒体准入同时受 PCM 与 DSP 预算约束。

独立移调采用 Phaze 风格的频谱峰区域移动：4096 点 FFT、128 帧 hop、32 重叠，分析和合成均使用 `0.8*(1-cos(2*pi*i/4096))` 窗。五 bin 邻域识别峰值；峰位置按 `round(peak_bin*pitch_ratio)` 移动，影响区域保持相对 bin 距离，并按累计 hop 相位旋转。负频率边界、Nyquist 边界、共轭频谱与逆变换归一化由原生实现显式处理。

该窗的平方重叠加权在不改变频谱时约为 0.96；独立移调湿声路径保留这一增益特征。整条路径未加入自动增益补偿或压缩器。稳定独立纯音仍受到整数峰 bin 的网格量化影响：在峰识别为最近 bin 的条件下，倍率 0.5 至 1.5 的理想频率误差估计上限约为 `0.75*sample_rate/4096`，22050 Hz 和 48000 Hz 分别约为 4.04 Hz 和 8.79 Hz。多峰、瞬态及跨影响区域的信号按实际输出评估；该公式描述特定纯音条件下的网格误差。

初始速度为 1、独立移调为 1 时保持直接样本路径，平直均衡器和单位增益下保持输入 f32 样本。启用独立移调后，音频和标签进入 4096 帧延迟路径；回到倍率 1 使用已延迟的干声，保持所有未输出内容，直到流或 seek 重置清除该延迟。干湿切换约为 20 ms，频谱更新的过渡余量最长为一个 4096 帧窗口，具体余量通过状态查询。

变速状态报告当前方法的名义输出前瞻：WSOLA 为 `ceil((frame_length+search_length)/playback_rate)`，非保调 sinc 路径为 `ceil(16/playback_rate)`，速度 1 为零。移调延迟通常为 4096 输出帧；设备切换后的准备输出队列增加的帧数计入查询延迟。4096 帧分别对应 22050 Hz 下约 185.76 ms、48000 Hz 下约 85.33 ms。

## 暂停、seek、预加载与设备切换

暂停冻结未输出的移调内容与标签，停止源消费和原始媒体时钟推进；恢复后继续输出保留内容。静音在最终增益阶段置零，继续消费有效媒体与推进时钟。

seek 建立新的 PCM epoch，清除变速输入、输出、重叠状态、移调历史及待输出旧标签，提交新的原始媒体基准。恢复输出前校验 epoch，旧标签不能覆盖新的位置。媒体替换和取消使用同样的内容边界。

预加载使用同一解码器处理路径与带标签 PCM。设置目标同时更新前台和已准备解码器；已有帧保留其生成时的设置。预加载提升保持同一 PCM 消费者和源许可，确认状态由提升后真正输出的标签推进。

设备切换保持同一媒体 token、PCM 消费者和 boxed 渲染状态。解码侧变速继续使用源格式；输出拥有者在回调之外准备新的采样率、均衡器和移调计划，并重采样、重放尚未输出的干声区间及标签。接入新回调前完成旧回调交还。配置或预算准入失败保留已有处理状态；设备状态通过[音频输出接口](audio-output.md) 查询。

解码输入 EOF 后，变速处理器先完成有限输出，再结束 PCM 发布。独立移调以有界零输入排出保留内容；EOF 补零不推进原始媒体位置。播放结束需等待最后有效标签和待输出内容完成，最终位置保持原始媒体结束位置。

## 后端与 CLI

使用 `lx-native-cli --audio memory --json-lines` 或 `--audio device`，每行提交一个 JSON 对象：

```json
{"request_id":"processing-1","command":"set_playback_processing","settings":{"playback_rate":1.5,"preserves_pitch":true,"pitch_ratio":0.75}}
{"request_id":"rate-1","command":"set_playback_rate","rate":2}
{"request_id":"preserve-1","command":"set_preserves_pitch","enabled":false}
{"request_id":"pitch-1","command":"set_pitch_ratio","ratio":1.25}
{"request_id":"processing-reset-1","command":"reset_playback_processing"}
```

五种命令返回 `committed`。`origin` 和 `expected_revision` 沿用统一命令封装。相同请求 ID 的相同输入在会话内返回原结果；不同输入复用该 ID 返回冲突。失败后的新尝试使用新的请求 ID。

查询保存目标与渲染确认状态：

```json
{"query":"playback_processing_settings"}
{"query":"playback_processing_state"}
```

设置响应示例：

```json
{"ok":true,"result":{"kind":"playback_processing_settings","data":{"playback_rate":1.5,"preserves_pitch":true,"pitch_ratio":0.75}}}
```

目标已提交、尚无有效输出确认时的状态结构：

```json
{
  "ok":true,
  "result":{
    "kind":"playback_processing_state",
    "data":{
      "desired":{"playback_rate":1.5,"preserves_pitch":true,"pitch_ratio":0.75},
      "applied":{"playback_rate":1,"preserves_pitch":true,"pitch_ratio":1},
      "requested_sequence":2,
      "applied_sequence":0,
      "latency_frames":0,
      "transition_remaining_frames":0,
      "sample_rate":null
    }
  }
}
```

普通快照及 `snapshot_cursor` 的完整快照通过 `playback_processing` 字段提供同一结构。目标、实际设置、请求/确认序号、采样率、延迟或过渡活动状态变化时发布 `playback_processing_changed`。事件的 `origin="audio"`、`operation_id=null`，`payload` 包含 `event="playback_processing_changed"` 和完整 `state`。

```json
{"query":"snapshot_cursor"}
{"query":"events","cursor":12}
```

先取得快照及配套游标，再在同一运行实例中查询其后的事件。事件不按每个 PCM 帧发布；精确当前余量使用状态查询。游标过期时重新取得完整快照。

## 存储与回调边界

变速输入历史、输出 hop、窗、重叠及 sinc 表采用固定存储。FFT 计划、查询所得 scratch、频谱、干声与标签队列均在渲染回调之外准备，使用 RustFFT 的预分配 scratch 处理接口。变速和移调共用进程级 64 MiB DSP 预算；固定分配按实际容量计入，FFT 计划使用保守预留额。前台、预加载、退休和候选状态在实际销毁前持续占用预算，预约字段在所拥有的存储之后销毁。PCM 环另受其自身预算约束。

数据回调使用固定帧、FFT scratch、原子控制与有界标签队列，不调用规划器，不持有设置互斥锁，不分配或释放 DSP/PCM 堆对象。设置邮箱合并最新目标；回调读取次数有界。回调析构将同一渲染状态交还输出拥有者，由拥有者处理源、计划和队列的销毁。解码工作线程与输出拥有者使用现有已登记工作线程；关闭等待实际剩余期限，超时保留拥有权以便后续完成回收。

权威实现入口为 `src/domain/playback_processing.rs`、`src/audio/processing.rs`、`src/audio/decode.rs`、`src/audio/pcm.rs`、`src/audio/pitch.rs`、`src/audio/output.rs` 和 `src/audio/output/device_owner.rs`。独立移调的上游参考为 [Phaze](https://github.com/olvb/phaze)，FFT scratch 接口见 [RustFFT 6.4.1](https://docs.rs/rustfft/6.4.1/rustfft/trait.Fft.html)。相关均衡器状态见[十段均衡器接口](equalizer.md)。
