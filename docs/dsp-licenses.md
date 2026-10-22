# DSP 组件许可与算法归属

本页覆盖原生 FFT 处理、对应数值依赖闭包、独立移调算法和 Chromium 音频算法参考。组件版本以 `Cargo.lock` 为准；运行组件与构建工具分别列出。

## Chromium 音频算法

固定算法基线为 Chromium `148.0.7778.280`。压缩器、卷积归一化、等功率空间声与距离处理使用相应 Blink 音频实现作为参考；原生实现增加固定存储、资源预算、媒体标签及设备交接。

| 原始组件 | 保留的完整版权与许可文本 |
| --- | --- |
| `dynamics_compressor.cc` | [原始许可](../licenses/third-party/chromium-audio-148.0.7778.280/dynamics_compressor.cc.LICENSE) |
| `reverb.cc` | [原始许可](../licenses/third-party/chromium-audio-148.0.7778.280/reverb.cc.LICENSE) |
| `equal_power_panner.cc` | [原始许可](../licenses/third-party/chromium-audio-148.0.7778.280/equal_power_panner.cc.LICENSE) |
| `distance_effect.cc` | [原始许可](../licenses/third-party/chromium-audio-148.0.7778.280/distance_effect.cc.LICENSE) |
| `audio_utilities.cc` | [原始许可](../licenses/third-party/chromium-audio-148.0.7778.280/audio_utilities.cc.LICENSE) |

保留文本包含各组件的原始 Google／Apple 版权声明、源代码与二进制再分发条件及免责声明。固定源路径位于 `third_party/blink/renderer/platform/audio`；完整上游许可继续随程序分发。

## FFT 与数值组件

| 组件 | 锁定版本 | 上游项目 | 许可证声明 | 保留文本目录 |
| --- | --- | --- | --- | --- |
| RustFFT | 6.4.1 | [ejmahler/RustFFT](https://github.com/ejmahler/RustFFT) | MIT OR Apache-2.0 | [rustfft-6.4.1](../licenses/third-party/rustfft-6.4.1/) |
| primal-check | 0.3.4 | [huonw/primal](https://github.com/huonw/primal) | MIT OR Apache-2.0 | [primal-check-0.3.4](../licenses/third-party/primal-check-0.3.4/) |
| strength_reduce | 0.2.4 | [ejmahler/strength_reduce](https://github.com/ejmahler/strength_reduce) | MIT OR Apache-2.0 | [strength_reduce-0.2.4](../licenses/third-party/strength_reduce-0.2.4/) |
| transpose | 0.2.3 | [ejmahler/transpose](https://github.com/ejmahler/transpose) | MIT OR Apache-2.0 | [transpose-0.2.3](../licenses/third-party/transpose-0.2.3/) |
| num-complex | 0.4.6 | [rust-num/num-complex](https://github.com/rust-num/num-complex) | MIT OR Apache-2.0 | [num-complex-0.4.6](../licenses/third-party/num-complex-0.4.6/) |
| num-integer | 0.1.47 | [rust-num/num-integer](https://github.com/rust-num/num-integer) | MIT OR Apache-2.0 | [num-integer-0.1.47](../licenses/third-party/num-integer-0.1.47/) |
| num-traits | 0.2.19 | [rust-num/num-traits](https://github.com/rust-num/num-traits) | MIT OR Apache-2.0 | [num-traits-0.2.19](../licenses/third-party/num-traits-0.2.19/) |
| autocfg，构建工具 | 1.5.1 | [cuviper/autocfg](https://github.com/cuviper/autocfg) | Apache-2.0 OR MIT | [autocfg-1.5.1](../licenses/third-party/autocfg-1.5.1/) |

各目录保存组件发行包中的完整 `LICENSE-MIT` 与 `LICENSE-APACHE`，包括原版权声明。RustFFT 用于原生移调的预规划 FFT 与预分配 scratch 计算；其数值依赖由锁定依赖图连接。`autocfg` 服务于数值库的构建配置检测。

MIT 文本中的版权归属分别为 The RustFFT Developers（RustFFT、strength_reduce）、Huon Wilson（primal-check）、The transpose Developers（transpose）、The Rust Project Developers（num-complex、num-integer、num-traits）和 Josh Stone（autocfg）。

## Phaze 算法参考

原生 `src/audio/pitch.rs` 使用 Rust 实现 Phaze 风格的频谱峰区域移调。参考链为 [olvb/phaze](https://github.com/olvb/phaze) 与 LX Music Desktop v2.12.6 固定提交 `ad95d5091c9ed689fa72b5e5c849df65f5a679ce` 中的 [phase-vocoder.js](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/renderer/plugins/player/pitch-shifter/phase-vocoder.js) 和 [ola-processor.js](https://github.com/lyswhut/lx-music-desktop/blob/ad95d5091c9ed689fa72b5e5c849df65f5a679ce/src/renderer/plugins/player/pitch-shifter/ola-processor.js)。

窗函数、五 bin 峰识别、影响区域划分、整数峰位移动及相位修正采用该参考设计。原生实现增加固定存储、最多八声道、原始媒体标签、延迟内容保留、token/epoch 围栏、有限 EOF 排空、设备格式重定向和预算管理；FFT 运算由 RustFFT 承担。参考 JavaScript 文件及其 `fft.js` 不作为本项目运行源码分发。

Phaze 的完整 Unlicense 文本保存在 [licenses/third-party/phaze/LICENSE](../licenses/third-party/phaze/LICENSE)。LX Music Desktop 的根许可和中文、英文、RTF 补充文本继续分别保存在 [licenses/upstream](../licenses/upstream/)；Phaze 文本不替代这些上游材料。

项目归属见 [NOTICE](../NOTICE)，播放处理行为见[倍速、保调与独立移调接口](playback-processing.md)。
