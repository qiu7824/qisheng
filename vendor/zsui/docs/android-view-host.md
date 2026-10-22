# Android retained View host

Enable the `android` feature plus the controls used by the application. The
`AndroidViewHost` adapter owns the shared retained View input runtime. The Java
`io.zsui.android.ZsuiView` draws its immutable frame through Android Canvas and
routes physical touch coordinates back to the same Rust layout and typed
messages. It does not use a WebView.

Create an `AndroidViewHost` from a `native_window(...).stateful_view(...)`
builder on the Android main thread. The application JNI entry owns its host and
converts JSON events to `AndroidViewEvent`. Application view/update code remains
ordinary typed Rust. A `UiInvalidationHandle` requests rebuilds when application
state changes; the Android owner calls `Refresh` on its main thread.

`Resize` carries physical width/height, Android density and font scale. Touch
coordinates use the same physical coordinate system. Only one pointer owns a
gesture; cancellation and Activity suspension release capture. The Java host
cancels a pending click before delivering vertical drag scrolling. Background
views reject pointer input and release the retained runtime's idle resources.

The `zsui.android.frame/v1` renderer supports semantic text, fills, rectangles,
rounded rectangles, arcs, polygons, polylines, linear gradients, raster images and balanced
clipping. Typeface fallback and glyph drawing use Android's text stack. Unknown
draw commands return an explicit error. Semantic icons, secure
text, a complete IME connection and semantic accessibility nodes are not yet
implemented in this adapter.

Raster commands reference immutable `ZsImageFrameId` resources. Frames carry
unpremultiplied ARGB pixels only for images absent from the receiver's retained
set. Before dispatching a Java View event, the JNI adapter passes that event's
`retained_images` string IDs to `AndroidViewHost::set_retained_images`.
Acknowledgement is receiver-driven: application service events can discard a
frame without losing the next image upload. A new View starts with an empty
set. The Java renderer retains recently used images within a fixed budget.
Each frame permits at most 256 raster resources and 8 MiB of decoded pixels.
Applications should additionally bound decoding and their source image cache.
Horizontal drags retain pointer capture; vertical drags cancel clicks and
dispatch scroll input. `AndroidInteractions` declares controlled viewport ranges
and optional Canvas swipe areas in surface-local DP. Supply it after layout with
`AndroidViewHost::dispatch_with_interactions`, or update a fixed declaration with
`set_interactions`. Scroll areas have stable IDs, an explicit offset and maximum;
later areas win when bounds overlap. The Java host uses platform velocity and
`OverScroller` physics, schedules motion on display frames, and emits the same
typed scroll route as dragging. It stops at bounds, on new touch, focus loss,
Activity suspension, surface changes or removal of the active area.

Change the declaration's `context` when the page or region meaning changes.
Both hosts cancel stale capture and kinetic state. A tap that stops a fling does
not activate a row. Multiple-pointer input cancels the active gesture.

Canvas clients use `on_canvas_swipe_with` and `ZsCanvasSwipeEvent` to receive
`ZsSwipeDirection::Left` or `Right`. Recognition uses logical displacement and
horizontal dominance; short, diagonal, cancelled and out-of-bounds drags do not
activate a swipe or a click. `AndroidSwipeArea::exclusions` preserves ordinary
pointer capture for sliders and other drag controls. Swipe callbacks require
the `canvas` feature; applications retain ownership of navigation and commands.

For example, a custom viewport can declare:

```rust
use zsui::{Dp, WidgetId};
use zsui::android_view::{AndroidInteractions, AndroidScrollArea, AndroidSwipeArea, AndroidTouchRect};
let bounds = AndroidTouchRect::new(Dp::new(0.0), Dp::new(80.0), Dp::new(400.0), Dp::new(500.0));
let regions = AndroidInteractions {
    context: 1,
    scrolls: vec![AndroidScrollArea { id: 7, rect: bounds, offset: Dp::new(0.0), max_offset: Dp::new(12000.0) }],
    swipes: vec![AndroidSwipeArea::new(WidgetId::new(7), bounds)],
};
```

The receiver retains recently used raster images within its existing 8 MiB /
256-resource bound, evicting only images absent from the current frame. This
avoids repeated uploads during back-and-forth scrolling without growing the
cache with the data set. Text layout uses an LRU of at most 256 entries; strings
over 4096 characters bypass it. Keys include geometry, typography, color and
wrapping, and configuration changes invalidate layouts. Each cached layout owns
its text paint, so subsequent draws cannot change its style.

The broader Activity bridge described by `mobile_scaffold_manifest` has a
separate completion gate covering all platform capabilities, including IME and
clipboard. Its scaffold status is not evidence against the narrower retained
View host, and retained View tests do not satisfy that broader gate.

Application services, media sessions, storage permissions and background audio
belong to the application. The View host must not own a player, database or
foreground service. An Activity can suspend its UI while an application media
service continues independently.

Validation requires both Rust input tests and an installed Android artifact:
visible rendering, real touch input, pause/resume lifecycle, and the relevant
application behavior. Desktop compilation does not establish Android runtime
completion. Device reports must identify the APK hash, API level and ABI.

## Scene transitions

`ZsuiView.setSceneTransition(key, SceneMotion)` is an optional Android host
presentation API. Call it in the Driver before returning a replacement frame.
Stable keys do not restart animation. `FADE`, `SLIDE_LEFT` and `SLIDE_RIGHT`
use a 220 ms platform animation, respect disabled system animations, and keep
at most one outgoing raster snapshot capped at 4 MiB. Navigation decisions
remain application-owned; interpolation does not dispatch repeated Rust events.
Touch during motion settles the transition and consumes that gesture so stale
visual positions cannot activate a different target. Resize, focus loss,
suspension, detachment and a replacement scene release the transient snapshot.

## Compact raster transfer

The Java receiver declares `raster_encoding: "argb_base64"` on View events.
JNI adapters select `AndroidRasterEncoding::ArgbBase64` with
`AndroidViewHost::set_raster_encoding` after reading that capability. Legacy
adapters continue to receive the default `argb` integer array. Compact resources
carry `argb_base64`, standard base64 of four unpremultiplied A/R/G/B bytes per
pixel. The receiver validates the encoded and decoded lengths before creating
a bitmap. Alpha semantics, receiver acknowledgements and the existing 8 MiB
per-frame budget are unchanged. The compact representation avoids one JSON
number allocation and conversion per pixel during first cover display.

The View also supplies bounded platform ripple feedback for touch-down. Its
color can follow the application palette through `setTouchFeedbackColor`.
Scrolling, pointer cancellation, focus loss, resizing and suspension settle the
feedback; it does not dispatch extra Rust events or run an idle animation loop.

## Progressive text highlighting

The Android receiver accepts an optional `highlight` decoration on a `text`
command: `{ "color": <ARGB>, "ranges": [{ "start": 0, "end": 2,
"progress": 0.5 }] }`. Indices are UTF-16 boundaries in that command's text;
progress is finite and bounded to 0 through 1. Up to 256 ranges and 4096 UTF-16
units are accepted. Invalid indices, split surrogate pairs and invalid progress
are rejected. Applications may decorate their own serialized Canvas frames.

The host derives masks from the existing shaped `StaticLayout`, retains the
original glyph positions and handles the selection direction of RTL text.
Completed ranges and partial ranges share a single overlay draw. The cached
layout paint is restored after drawing. Highlight progress does not enter the
layout cache key. Content, timing, playback and update scheduling remain owned
by the application; the framework does not infer timing or run an idle timer.
