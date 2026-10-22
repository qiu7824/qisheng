# Audio spectrum query

```json
{"query":"audio_analysis"}
```

The query returns the latest spectrum from resampled source PCM before output channel mapping, equalization, pitch shifting, gain and mute. Both memory and device output use the same render tap. FFT size is 256 and the result contains 128 byte bins and 128 finite decibel values.

Analysis is disabled by default. `set_audio_analysis_enabled` with `enabled:true` enables sampling; `audio_analysis_enabled` queries the persisted preference. Disabled sampling runs no FFT and `audio_analysis` returns null. Configuration changes reset analysis history, and a frame from an earlier enable sequence cannot reappear after a rapid disable/enable cycle.

The result includes the output sample rate, channel count, mix rule, valid window length, accepted PCM frame count, stream token, source epoch and original media coordinate. The byte scale uses −100 dB to −30 dB and smoothing is 0.8. Exact zero is represented by −1000 dB for JSON interchange.

Analysis runs at most once per sample-rate/30 accepted-frame interval. Paused output and unavailable PCM do not manufacture new frames or apply smoothing decay. Accepted zero PCM updates the spectrum normally. Seeking or installing another stream resets the window and smoothing; frames from earlier stream tokens and source epochs are excluded from the current output observation.

Mono, stereo, quad and 5.1 use the analyser speaker mix. Native 3, 5, 7 and 8 channel layouts use the explicitly reported arithmetic average. The reported channel count describes the source layout, so a 5.1 LFE channel is excluded by the speaker mix even when the selected output is mono.

FFT plans and scratch storage are prepared outside the device callback. The render owner retains the analyser and returns it through the existing callback retirement queue. Publication uses fixed atomic storage; the callback does not serialize JSON, allocate a result vector or take the query mutex. The audio engine copies observations into the query boundary and checks generation, control intent and seek sequence before returning them.
