# 流式播放、缓存与预加载架构

范围：M05 流式播放与 M06 音频处理，开发源码 0.3.0-alpha.11。生产播放使用块传输与独立解码；68 MiB 原创 WAV、第二块持续传输21秒及实际 Memory PCM/seek 已通过专项验证。跨平台匹配与URL信息缓存已接入，持久图片与跨播放编码复用已有执行链路，M05仍包含本地匹配、其他错误恢复分支与完整服务联测；前端布局属于后续阶段。

## 线程与执行链

`Backend actor → AudioTransport → StreamMediaSource → DecoderSession → 倍速保调／变速变调 → 带标签 PcmBridge`。

`PcmBridge → 输出采样率／声道转换 → 十段 EQ → 独立 pitch → 音量／静音 → 限幅 → Device / Memory`。

网络与文件等待以及倍速处理发生在传输/解码线程。输出回调读取预分配、有界、非阻塞的 PCM 队列并执行已准备的 EQ 与 pitch，不能执行网络、磁盘读取、解码、FFT 规划或同步 seek。设备与 Memory 使用同一 PCM source。

PCM 帧携带原始媒体结束坐标、Q0.32 小数、处理设置、序号与 epoch。解码预取和转换器预取不提交呈现时钟；时钟随经过后级延迟并在音量／静音之后接受的有效标签推进。暂停、启动补零、没有有效标签的断流补零及定位等待不推进；静音有效帧正常推进。已有延迟内容可在输入暂缺时继续送出，输入队列为空不单独表示输出断流或播放结束。设备声学延迟由 M06 实机测量。

解码层使用显式 Symphonia 0.5.5 session，区分正常结束、网络中断、格式/解码错误。输出使用直接 CPAL 0.17.3 与固定容量 rtrb 所有权通道；Device 和 Memory 共享采样率/声道转换内核。PCM 与 source 的分配、销毁发生在拥有者线程，输出数据回调不调用应用锁、文件系统、解码或堆分配。

| 模块 | 责任 |
| --- | --- |
| audio/transport | Tokio HTTP、Range、响应期限、有限重试和表示版本 |
| audio/cache | 块级 read/publish、LRU、配额、活动 pin、cache epoch 与延迟清理 |
| audio/media_source | Read/Seek/MediaSource；仅解码线程可以等待缺块；暂缺字节不能返回 Read=0 |
| audio/decode | 格式探测、处理后的 PCM 输出、准确 seek、decoder reset、丢弃目标前帧 |
| audio/processing | 解码线程上的 WSOLA 保调倍速与有限带限变速变调、原始坐标标签 |
| audio/pcm | 固定容量 SPSC、不可变帧标签、延迟呈现时钟、epoch 丢弃、真实输入 EOS 和错误侧带 |
| audio/dsp | 实际输出采样率的十段 EQ 计划及两组固定滤波历史 |
| audio/pitch | 预规划的有限 FFT／OLA 独立移调、固定延迟、标签队列与格式重放 |
| audio/output | 转换与 DSP 顺序、有效标签提交、原子暂停／增益、RT source 交接、有限 EOF 排空与拥有者回收 |
| audio/prepared | 唯一投机 decoder/transport/source、不可复制 token、nonce/代际诊断和真实输出提升 |
| audio/http_client | 限定 HTTP/1、TLS验证、独立8KiB协议/DATA边界；无代理、重定向、cookies或解压 |
| playback/preload | 队列预选、已批准源解析、独立 URL 审批、准备、promotion 与撤销 |

默认配额为256 KiB块、encoded RAM 8 MiB、全局 PCM 8 MiB、磁盘512 MiB；预加载累计 HTTP DATA最多2 MiB，默认PCM队列80 ms/预填40 ms，最高250 ms。PCM全局限额包含 codec整包 scratch；scratch 的时长独立于队列时长。活动decoder 1、预加载decoder 1、取消收尾资源最多4组，对象与块数量同时有限。控制 p95目标100 ms、正常关闭目标5秒，大缓存文件系统条件仍待验收。

倍速与独立 pitch 的固定缓冲、FFT 计划／scratch、原始坐标标签及重放存储共享独立的 64 MiB DSP 配额。规划与格式转换在拥有者线程预先预约，准备失败保持原端点和待输出内容；配额在相关存储实际销毁后释放。

投机预算累计计算应用交付的 HTTP DATA，包括失败重试与头尾定位读取。每次body poll先预留8 KiB，可靠Content-Length末帧按剩余长度预留；零剩余允许确认EOF。TCP/TLS内核预收不属于此计量，HTTP协议缓冲独立固定8 KiB/响应头64项。预算暂停不触发网络idle，取消或同lease提升唤醒等待；不能容纳完整缓存块的预算在发起请求前拒绝。

## HTTP 表示一致性

初始 GET 使用 `Range: bytes=0-262143` 与 identity 编码。HEAD 仅辅助，不能证明 Range 支持。沿用明确代理与重定向策略；新 URL 和授权变化重新校验。

| 情况 | 行为 |
| --- | --- |
| 206 | 校验 Content-Range 起止、total、实际 body 长度；完整块才发布有效范围 |
| 200 忽略 Range | 从绝对零作为单响应顺序流；不拼到请求偏移；必要时废弃旧表示并重建 decoder |
| 强 ETag | 后续 If-Range；变化提升 stream epoch，取消旧字节和旧 PCM |
| 无或弱 validator | 单响应顺序模式；弱 ETag 不作 If-Range，不拼接跨请求旧块 |
| 416 | 解析 bytes */N；同表示且 offset≥N 才是 EOF；缩小或 offset<N 为变化/协议错误 |
| 未知长度 | byte_len=None；SeekFrom::End 明确不支持；正常 EOF 后锁定长度 |
| 短响应/断开 | Network/Incomplete；强 validator 下有限续取，否则有限重开顺序流 |
| 时限与背压 | connect、响应头及期待数据期间 idle timeout；主动暂停背压不计网络 idle |
| 不支持格式或受保护内容 | 明确 Unsupported/Decode，不增加音频 DRM 解密路径 |

Range、If-Range 与 validator 规则见 [RFC 9110](https://www.rfc-editor.org/rfc/rfc9110.html#section-13.1.5)。M4A 尾部 moov 可要求尾 Range；未知长度且无 Range 时允许等待顺序流，不伪造即时 seek。

独立块文件与有效区间表用于区分缓存空洞和真实音频。超过配额的文件使用滑动窗口，不固定整首。清理仅作用于专属缓存根内注册的受管块：提升 cache epoch、取消预加载、被 lease 使用的块延期清除。原音频、正式下载、列表、编辑歌词及源批准数据均不属于缓存清理范围。

## 审批与代际边界

ApprovedAudioRequest 记录 track、platform、actual_quality、resolver SHA/策略/epoch、精确 URL 摘要、approval ID/epoch。缓存键绑定曲目、实际质量、resolver 和表示版本；日志不输出签名查询串。同 URL Range 沿用有效许可，重解析/重定向不能扩大许可。

seek 提升 decode epoch，输出丢弃旧 epoch；worker 准确定位和预填成功后才提交 seek_seq/新位置，保留暂停意图。Stop/换曲/关闭取消 HTTP 并唤醒等待，由拥有者收尾，控制线程不同步 join 慢任务。

预加载直接使用当前queue.preview_next(Ended, eligible)，保留随机预选和临时FIFO。duration>10秒且剩余<10秒、定位后至少间隔3秒时触发。fence绑定operation epoch、当前播放generation/entry、queue revision、候选entry、source SHA/epoch、候选实际质量和源approval epoch；精确URL审批另记录URL摘要及approval epoch。

预加载authority是Source或CatalogAsset闭合类型。Source携带脚本SHA与策略版本；CatalogAsset携带已登记资产ID及size/mtime/fingerprint的内容stamp，质量固定local。资产原有文件批准绑定精确catalog URI，不创建脚本SHA或策略版本；重新导入、来源变化及队列修改使旧准备失效。

未批准URL停在AwaitingApproval，媒体请求计数为零；预加载许可与当前pending URL分开。真实advance后验证fence并原子提升token，同一engine搬移现有decoder、transport与PCM source。异步结果还绑定nonce和当前generation：输出成功附着并实际消费内容帧，或确认暂停/真实EOF后才发布Promoted。资源变更、取消或输出失败回到候选曲的正常解析与审批链路。模式、列表、Dislike、插队、质量、源、seek、Stop及Clear均撤销旧预加载；投机失败不修改当前音频错误或跳过集合。

单个音源使用一个JS会话，播放解析优先于投机解析与local歌词。operation-scoped IPC取消会清理该操作的HTTP、timers及Promise jobs并保留会话闭包；停用、删除、换源和关闭终止进程。同步JavaScript/native代码仍受既有CPU/迭代限额约束，协作取消不会硬抢占正在执行的同步调用。

当前播放视图以 stream serial 保持独立归属；持久编码块以解析上下文、精确 URL、原曲、实际平台／质量、源 SHA／策略及块格式组成稳定数据键。跨播放和冷进程重开均须先取得当前 URL 批准，再验证相同强 ETag、总长度及完整块 SHA；忽略 Range 的 200 仅复用受保护的完整表示，见[持久编码复用](encoded-reuse.md)。两槽登记、OS owner 锁与块 ownership header 支持死亡进程恢复和可信持久块认领。未知文件、替换 header 及 reparse 目标保留并以 cannotClear 报告。物理磁盘统计包含管理元数据，encoded_bytes 归零后 disk_bytes 可以仍非零。缓存发布、登记、审核和回收由独立有界 I/O worker 负责；控制端保持短锁与实际 join 关闭语义。

异步初始化和发布通过有界队列执行，待发布byte总量单独计量。退休文件在物理unlink与登记提交前仍占全quota；clear回执等待实际审核与可清块回收。独立reader hazard与不可变ownership proof支持锁外磁盘读。Engine/decoder/cache/Runtime在spawn前预约有界owner，正常Runtime Drop在其拥有者线程；关闭超时保留真实handle供查询和重join。外部Drop仅取消/唤醒，不进行FS或无期限join。

## 集成验收

真实 loopback HTTP 与生产 Memory 输出验证：大于64 MiB 原创 WAV 首 PCM 早于传输完成；超过20秒持续传输；中段/尾段 seek 的频率；206/200/416、坏范围、ETag变化及未知长度；暂停、断流、正常结束；Stop/切歌/A→B→A/连续 seek 与关闭无遗留线程。预加载验证同一候选、临时 FIFO、未审批零请求和撤权取消。小配额/活动清理/磁盘满验证保持原文件与用户数据。Device 输出另作真实硬件验收。

## 设备拥有者与热切换

M06设备拥有者承担枚举、建流、关闭及重连，回调通过有界队列交还唯一渲染状态。候选的 pitch 计划与重放容量在隔离旧端点之前准备；配额不足不停止原活动端点。候选确认后移交原 PCM 消费者、预取帧及未输出的 pitch 样本／标签，按实际格式调整转换器并在回调之外转换重放内容，保持呈现时钟、token 和 seek epoch。准备好的输出先送出，不重复拉取源帧。

暂停与切换冻结 pending pitch 队列；尚未接受的已取出呈现帧保存在单个固定槽内。源暂停先于输出暂停到达时同样保留这些内容；seek、token／epoch 改变、源错误和关闭清除失效内容。设备选择、静音及拓扑策略不重新解析已批准 URI，也不清除缓存；输出准备期间保存 DecoderReady 而不向未就绪格式附加 PCM。关闭超时保留真实拥有者供后续 join，完整接口与依赖边界见[设备接口](audio-output.md)。

## 十段EQ处理

输出侧处理顺序为采样率／声道转换、十段 EQ、独立 pitch、音量／静音和最终限幅。EQ 系数在引擎或设备拥有者准备，以固定原子 mailbox 发布；回调有限次数读取，使用两组固定 f64 历史处理同一原始帧，以约 20 ms 过渡并合并更晚参数。seek、来源更换、暂停、设备隔离及包含断流的预取帧清除 EQ 历史。EQ 自身不增加 EOF 尾音，后级 pitch 的有限排空仍须完成，详见[均衡器接口](equalizer.md)。

## 播放处理、延迟与结束标签

`PlaybackProcessingSettings` 默认速率 1、保调开启、独立 pitch 比率 1；速率范围 0.5–2，比率范围 0.5–1.5。保调倍速在解码线程执行 WSOLA；关闭保调时使用有限带限变速变调。独立 pitch 位于 EQ 之后，以 4096 点 FFT、128 帧 hop 的峰区移动与 OLA 处理各声道；单位比率初始直接旁路，启用移调后采用 4096 个输出帧的常规延迟。返回单位比率保留未输出内容及延迟，直至新媒体或 seek 重置，完整控制接口见[播放处理接口](playback-processing.md)。

处理快照中的 `sample_rate` 是实际输出采样率，`latency_frames` 为该速率下的帧数：源倍速延迟先换算到输出单位，再加 pitch 延迟和格式切换的待重放队列。转交 PCM 呈现标签时，延迟换算回源 PCM 帧单位；原始媒体坐标始终按源 PCM 采样率换算为时间，不因端点采样率改变而更换分母。实际速率和保调状态取自发出的不可变源标签，pitch 比率取自实际处理目标，确认序号取两个实际分量序号的较小值；混合过渡由剩余帧数明确表示。

真实输入 EOF 与输出结束分别管理。pitch 在输入结束后采用两倍窗口、即 8192 个零输入帧的有界排空预算，并优先送出准备好的重放与暂存帧；所有有效标签及有限排空完成后才发布输出结束。启动零样本和没有有效标签的排空帧不会推进原始媒体时钟，最终有效标签保留真实媒体终点。输入队列清空、DecoderReady 或设置提交回执不能替代这一呈现确认。

卷积、空间声、压缩器、分析器及系统媒体功能继续按 M06 推进。Memory 原创信号与受控端点验证不代表物理扬声器或目标设备实时 CPU 验收；硬件持续负载、驱动／声学延迟和主观听音分别记录。
