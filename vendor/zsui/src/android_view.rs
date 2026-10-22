//! Android host for the shared retained View runtime.
//!
//! The Android main thread owns this value. `android/ZsuiView` lowers its
//! serialized draw commands to a hardware-accelerated Android Canvas. Input
//! follows the same layout and typed message route as the desktop hosts.

use crate::native::{NativeViewInputRuntime, NativeWindowBuilder};
use crate::native_draw_support::NativeDrawPalette;
use crate::{
    Color, Dp, Dpi, NativeDrawCommand, NativeDrawPlan, Point, Rect, ZsuiError, ZsuiResult,
};
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashSet;
#[path = "android_interaction.rs"]
mod interaction;
pub use interaction::{
    AndroidInteractionFrame, AndroidInteractions, AndroidScrollArea, AndroidScrollFrame,
    AndroidSwipeArea, AndroidTouchRect,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AndroidViewEvent {
    Resize {
        width: i32,
        height: i32,
        density: f32,
        font_scale: f32,
    },
    Touch {
        phase: AndroidTouchPhase,
        pointer: i32,
        x: f32,
        y: f32,
    },
    Scroll {
        x: f32,
        y: f32,
        delta_y: f32,
    },
    Text {
        text: String,
    },
    Lifecycle {
        resumed: bool,
    },
    Refresh,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AndroidTouchPhase {
    Down,
    Move,
    Up,
    Cancel,
}

/// One immutable frame. Coordinates and text sizes are Android physical pixels.
#[derive(Debug, Clone, Serialize)]
pub struct AndroidViewFrame {
    pub schema: &'static str,
    pub width: i32,
    pub height: i32,
    pub background: u32,
    pub commands: Vec<Value>,
    pub interactions: AndroidInteractionFrame,
    /// Newly referenced raster resources, omitted by the receiver's next acknowledgement.
    pub images: Vec<Value>,
    pub text_input: Option<String>,
    pub handled: bool,
    pub messages: usize,
    pub errors: Vec<String>,
}

/// Receiver-negotiated raster payload. Legacy arrays remain the default.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AndroidRasterEncoding {
    #[default]
    ArgbArray,
    /// Standard base64 of unpremultiplied A, R, G, B bytes, four bytes per pixel.
    ArgbBase64,
}

pub struct AndroidViewHost {
    runtime: NativeViewInputRuntime,
    plan: NativeDrawPlan,
    width: i32,
    height: i32,
    density: f32,
    active_pointer: Option<i32>,
    swipe_start: Option<(Point, AndroidSwipeArea)>,
    interactions: AndroidInteractions,
    resumed: bool,
    retained_images: HashSet<u64>,
    raster_encoding: AndroidRasterEncoding,
}

impl AndroidViewHost {
    pub fn new(builder: NativeWindowBuilder) -> ZsuiResult<Self> {
        if builder.native_live_view_runtime().is_none() {
            return Err(ZsuiError::invalid_spec(
                "android_view",
                "a retained stateful View is required",
            ));
        }
        let plan = builder.native_draw_plan().cloned().unwrap_or_default();
        Ok(Self {
            runtime: builder.native_view_input_runtime(),
            plan,
            width: 1,
            height: 1,
            density: 1.0,
            active_pointer: None,
            swipe_start: None,
            interactions: AndroidInteractions::default(),
            resumed: true,
            retained_images: HashSet::new(),
            raster_encoding: AndroidRasterEncoding::default(),
        })
    }

    /// Reports the resources actually retained by this View instance. Call before
    /// dispatching its next event. Frames discarded by an application bridge do
    /// not acknowledge resources; a recreated View starts with an empty list.
    pub fn set_retained_images(&mut self, ids: &[crate::ZsImageFrameId]) {
        self.retained_images = ids.iter().take(256).map(|id| id.0).collect();
    }

    /// Select compact pixels only after the receiver declares support.
    pub fn set_raster_encoding(&mut self, encoding: AndroidRasterEncoding) {
        self.raster_encoding = encoding;
    }

    /// Updates controlled regions after layout. A context change cancels old capture.
    pub fn set_interactions(&mut self, interactions: AndroidInteractions) -> ZsuiResult<()> {
        interactions.validate()?;
        if self.interactions.context != interactions.context {
            self.active_pointer = None;
            self.swipe_start = None;
            if let Some(plan) = self.runtime.cancel_pointer_drag().redraw_plan {
                self.plan = plan;
            }
        }
        self.interactions = interactions;
        Ok(())
    }

    /// Dispatches one event and attaches regions derived from the resulting layout.
    pub fn dispatch_with_interactions(
        &mut self,
        event: AndroidViewEvent,
        regions: impl FnOnce() -> AndroidInteractions,
    ) -> ZsuiResult<AndroidViewFrame> {
        self.dispatch_inner(event, Some(regions))
    }

    pub fn dispatch(&mut self, event: AndroidViewEvent) -> ZsuiResult<AndroidViewFrame> {
        self.dispatch_inner(event, None::<fn() -> AndroidInteractions>)
    }

    fn dispatch_inner<F: FnOnce() -> AndroidInteractions>(
        &mut self,
        event: AndroidViewEvent,
        regions: Option<F>,
    ) -> ZsuiResult<AndroidViewFrame> {
        let report = match event {
            AndroidViewEvent::Resize {
                width,
                height,
                density,
                font_scale,
            } => {
                if !(1..=16384).contains(&width)
                    || !(1..=16384).contains(&height)
                    || !density.is_finite()
                    || !(0.5..=8.0).contains(&density)
                    || !font_scale.is_finite()
                    || !(0.5..=4.0).contains(&font_scale)
                {
                    return Err(ZsuiError::invalid_spec(
                        "android_surface",
                        "invalid size or scale",
                    ));
                }
                self.active_pointer = None;
                self.swipe_start = None;
                self.runtime.cancel_pointer_drag();
                self.width = width;
                self.height = height;
                self.density = density;
                self.runtime.set_typography_scale(font_scale);
                self.runtime.set_surface(
                    Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    },
                    Dpi::new(96.0 * density),
                )
            }
            AndroidViewEvent::Touch {
                phase,
                pointer,
                x,
                y,
            } => {
                let point = checked_point(x, y)?;
                if !self.resumed {
                    return self.finish_frame(false, 0, Vec::new(), regions);
                }
                match phase {
                    AndroidTouchPhase::Down if self.active_pointer.is_none() => {
                        self.active_pointer = Some(pointer);
                        self.swipe_start = self
                            .interactions
                            .swipe_at(x / self.density, y / self.density)
                            .map(|area| (point, area));
                        self.runtime.dispatch_pointer_down(point, false)
                    }
                    AndroidTouchPhase::Move if self.active_pointer == Some(pointer) => {
                        self.runtime.dispatch_pointer_move(point)
                    }
                    AndroidTouchPhase::Up if self.active_pointer == Some(pointer) => {
                        self.active_pointer = None;
                        let swipe = self.swipe_start.take();
                        #[cfg(feature = "canvas")]
                        if let Some((start, area)) = swipe {
                            let dx = (point.x - start.x) as f32 / self.density;
                            let dy = (point.y - start.y) as f32 / self.density;
                            if dx.abs() > 8.0 || dy.abs() > 8.0 {
                                let cancelled = self.runtime.cancel_pointer_drag();
                                if area.rect.contains(x / self.density, y / self.density)
                                    && dx.abs() >= area.minimum_distance.0
                                    && dx.abs() > dy.abs() * 1.4
                                {
                                    let direction = if dx > 0.0 {
                                        crate::ZsSwipeDirection::Right
                                    } else {
                                        crate::ZsSwipeDirection::Left
                                    };
                                    let mut report = self.runtime.dispatch_canvas_swipe(
                                        area.widget,
                                        start,
                                        point,
                                        direction,
                                    );
                                    report.message_count += cancelled.message_count;
                                    if report.redraw_plan.is_none() {
                                        report.redraw_plan = cancelled.redraw_plan;
                                    }
                                    if let Some(plan) = report.redraw_plan {
                                        self.plan = plan;
                                    }
                                    return self.finish_frame(
                                        report.handled,
                                        report.message_count,
                                        report.errors,
                                        regions,
                                    );
                                }
                                if let Some(plan) = cancelled.redraw_plan {
                                    self.plan = plan;
                                }
                                return self.finish_frame(
                                    cancelled.handled,
                                    cancelled.message_count,
                                    cancelled.errors,
                                    regions,
                                );
                            }
                        }
                        #[cfg(not(feature = "canvas"))]
                        let _ = swipe;
                        self.runtime.dispatch_pointer_up(point)
                    }
                    AndroidTouchPhase::Cancel => {
                        self.active_pointer = None;
                        self.swipe_start = None;
                        self.runtime.cancel_pointer_drag()
                    }
                    _ => return self.finish_frame(false, 0, Vec::new(), regions),
                }
            }
            AndroidViewEvent::Scroll { x, y, delta_y } => {
                let point = checked_point(x, y)?;
                if !delta_y.is_finite() || delta_y.abs() > 32768.0 {
                    return Err(ZsuiError::invalid_spec(
                        "android_scroll",
                        "invalid scroll delta",
                    ));
                }
                if !self.resumed {
                    return self.finish_frame(false, 0, Vec::new(), regions);
                }
                self.runtime
                    .dispatch_pointer_scroll(point, Dp::new(delta_y / self.density))
            }
            AndroidViewEvent::Text { text } => {
                if text.len() > 65536 {
                    return Err(ZsuiError::invalid_spec(
                        "android_text",
                        "commit is too large",
                    ));
                }
                self.runtime.dispatch_ime_commit(&text)
            }
            AndroidViewEvent::Lifecycle { resumed } => {
                self.resumed = resumed;
                self.swipe_start = None;
                self.active_pointer = None;
                self.runtime.cancel_pointer_drag();
                if resumed {
                    if let Some(plan) = self.runtime.resume_view_when_visible() {
                        self.plan = plan;
                    }
                } else {
                    self.runtime.suspend_view_when_hidden();
                }
                return self.finish_frame(false, 0, Vec::new(), regions);
            }
            AndroidViewEvent::Refresh => self.runtime.refresh_invalidated_view(),
        };
        if let Some(plan) = report
            .redraw_plan
            .or_else(|| self.runtime.current_composed_draw_plan())
        {
            self.plan = plan;
        }
        self.finish_frame(report.handled, report.message_count, report.errors, regions)
    }

    fn finish_frame<F: FnOnce() -> AndroidInteractions>(
        &mut self,
        handled: bool,
        messages: usize,
        errors: Vec<String>,
        regions: Option<F>,
    ) -> ZsuiResult<AndroidViewFrame> {
        if let Some(regions) = regions {
            self.set_interactions(regions())?;
        }
        self.frame(handled, messages, errors)
    }

    fn frame(
        &self,
        handled: bool,
        messages: usize,
        errors: Vec<String>,
    ) -> ZsuiResult<AndroidViewFrame> {
        let palette = NativeDrawPalette::for_mode(self.plan.theme_mode, false);
        let mut commands = Vec::with_capacity(self.plan.commands.len());
        let mut images = Vec::new();
        let mut referenced = HashSet::new();
        let mut image_bytes = 0usize;
        for command in &self.plan.commands {
            if let NativeDrawCommand::Image(image) = command {
                if referenced.insert(image.frame.id().0) {
                    image_bytes = image_bytes.saturating_add(image.frame.decoded_bytes());
                    if referenced.len() > 256 || image_bytes > 8 * 1024 * 1024 {
                        return Err(ZsuiError::invalid_spec(
                            "android_images",
                            "frame raster budget exceeds 8 MiB or 256 resources",
                        ));
                    }
                    if !self.retained_images.contains(&image.frame.id().0) {
                        let pixels: Vec<u32> = image
                            .frame
                            .premultiplied_bgra8()
                            .chunks_exact(4)
                            .map(|p| {
                                let alpha = u32::from(p[3]);
                                let channel = |c: u8| {
                                    if alpha == 0 {
                                        0
                                    } else {
                                        (u32::from(c) * 255 / alpha).min(255)
                                    }
                                };
                                (alpha << 24)
                                    | (channel(p[2]) << 16)
                                    | (channel(p[1]) << 8)
                                    | channel(p[0])
                            })
                            .collect();
                        let mut resource = json!({"id":image.frame.id().0.to_string(),"width":image.frame.width(),"height":image.frame.height()});
                        match self.raster_encoding {
                            AndroidRasterEncoding::ArgbArray => resource["argb"] = json!(pixels),
                            AndroidRasterEncoding::ArgbBase64 => {
                                let bytes: Vec<u8> =
                                    pixels.into_iter().flat_map(u32::to_be_bytes).collect();
                                resource["argb_base64"] = base64::engine::general_purpose::STANDARD
                                    .encode(bytes)
                                    .into();
                            }
                        }
                        images.push(resource);
                    }
                }
            }
            commands.push(lower(
                command,
                palette,
                self.density,
                self.plan.typography_scale(),
            )?);
        }
        Ok(AndroidViewFrame {
            schema: "zsui.android.frame/v1",
            width: self.width,
            height: self.height,
            background: argb(palette.surface),
            commands,
            interactions: self.interactions.frame(self.density),
            images,
            text_input: self.runtime.focused_text_input_value(),
            handled,
            messages,
            errors,
        })
    }
}

fn checked_point(x: f32, y: f32) -> ZsuiResult<Point> {
    if !x.is_finite() || !y.is_finite() || x.abs() > 32768.0 || y.abs() > 32768.0 {
        return Err(ZsuiError::invalid_spec(
            "android_pointer",
            "invalid pointer coordinates",
        ));
    }
    Ok(Point {
        x: x.round() as i32,
        y: y.round() as i32,
    })
}

fn argb(color: Color) -> u32 {
    u32::from_be_bytes([color.a, color.r, color.g, color.b])
}

fn lower(
    command: &NativeDrawCommand,
    palette: NativeDrawPalette,
    density: f32,
    font_scale: f32,
) -> ZsuiResult<Value> {
    let fill = |value| argb(palette.resolve_source_fill(value));
    Ok(match command {
        NativeDrawCommand::FillRect { rect, fill: color } => {
            json!({"op":"rect","rect":rect,"color":fill(*color)})
        }
        NativeDrawCommand::StrokeRect {
            rect,
            stroke,
            width,
        } => json!({"op":"stroke_rect","rect":rect,"color":fill(*stroke),"width":width}),
        NativeDrawCommand::StrokeArc {
            rect,
            stroke,
            width,
            start_degrees,
            sweep_degrees,
        } => {
            json!({"op":"arc","rect":rect,"color":fill(*stroke),"width":width,"start":start_degrees,"sweep":sweep_degrees})
        }
        NativeDrawCommand::RoundRect {
            rect,
            fill: color,
            stroke,
            radius,
        } => {
            json!({"op":"round","rect":rect,"color":fill(*color),"stroke":stroke.map(fill),"radius":radius})
        }
        NativeDrawCommand::RoundFill {
            rect,
            fill: color,
            radius,
        } => json!({"op":"round","rect":rect,"color":fill(*color),"radius":radius}),
        NativeDrawCommand::FillTriangle {
            points,
            fill: color,
        } => json!({"op":"polygon","points":points,"color":fill(*color)}),
        NativeDrawCommand::FillPolygon {
            points,
            fill: color,
        } => json!({"op":"polygon","points":points,"color":fill(*color)}),
        NativeDrawCommand::StrokePolyline {
            points,
            stroke,
            width,
            closed,
        } => {
            json!({"op":"polyline","points":points,"color":fill(*stroke),"width":width,"closed":closed})
        }
        NativeDrawCommand::FillGradient {
            rect,
            start,
            end,
            direction,
            radius,
        } => {
            json!({"op":"gradient","rect":rect,"start":fill(*start),"end":fill(*end),"direction":direction,"radius":radius})
        }
        NativeDrawCommand::PushClip { rect } => json!({"op":"clip","rect":rect}),
        NativeDrawCommand::PopClip => json!({"op":"restore"}),
        NativeDrawCommand::Image(image) => {
            json!({"op":"image","id":image.frame.id().0.to_string(),"rect":image.bounds,"source":image.source,"smooth": image.interpolation == crate::NativeImageInterpolation::Smooth})
        }
        NativeDrawCommand::Text(text) => {
            let metrics = text
                .style
                .role
                .metrics_for(crate::ZsPlatformStyle::current());
            let weight = if text.style.weight == crate::TextWeight::Automatic {
                metrics.default_weight
            } else {
                text.style.weight
            };
            json!({"op":"text","rect":text.bounds,"text":text.text,
                "color":argb(text.color.filter(|_| !palette.high_contrast).unwrap_or_else(|| palette.resolve(text.style.color))),
                "size":text.size.map(|size|size.dip()).unwrap_or(metrics.size) * density * font_scale,
                "weight":weight,"align":text.style.horizontal_align,"vertical":text.style.vertical_align,
                "wrap":text.style.wrap,"ellipsis":text.style.ellipsis,"family":text.family})
        }
        // Unsupported commands are reported explicitly instead of silently
        // substituting blank pixels or claiming a fully implemented renderer.
        other => {
            return Err(ZsuiError::invalid_spec(
                "android_draw_command",
                format!("unsupported command: {:?}", other.operation()),
            ));
        }
    })
}

#[cfg(all(test, feature = "button", feature = "label"))]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    fn host(count: Arc<AtomicUsize>) -> AndroidViewHost {
        AndroidViewHost::new(crate::native_window("Android input test").stateful_view(
            0usize,
            |_| {
                crate::button("Activate")
                    .id(crate::WidgetId::new(7))
                    .width(Dp::new(160.0))
                    .height(Dp::new(64.0))
                    .on_click(())
            },
            move |value, (), _| {
                *value += 1;
                count.store(*value, Ordering::SeqCst);
            },
        ))
        .unwrap()
    }

    #[test]
    fn native_touch_routes_through_the_retained_view_at_multiple_densities() {
        for density in [1.0, 1.5, 2.0, 3.0] {
            let count = Arc::new(AtomicUsize::new(0));
            let mut host = host(count.clone());
            let frame = host
                .dispatch(AndroidViewEvent::Resize {
                    width: (320.0 * density) as i32,
                    height: (600.0 * density) as i32,
                    density,
                    font_scale: 1.0,
                })
                .unwrap();
            assert!(frame.commands.iter().any(|v| v["op"] == "text"));
            for phase in [AndroidTouchPhase::Down, AndroidTouchPhase::Up] {
                host.dispatch(AndroidViewEvent::Touch {
                    phase,
                    pointer: 0,
                    x: 24.0 * density,
                    y: 24.0 * density,
                })
                .unwrap();
            }
            assert_eq!(count.load(Ordering::SeqCst), 1);
            host.dispatch(AndroidViewEvent::Touch {
                phase: AndroidTouchPhase::Up,
                pointer: 1,
                x: 24.0,
                y: 24.0,
            })
            .unwrap();
            assert_eq!(count.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn cancelled_or_background_touch_cannot_activate_controls() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut host = host(count.clone());
        host.dispatch(AndroidViewEvent::Resize {
            width: 320,
            height: 600,
            density: 1.0,
            font_scale: 1.0,
        })
        .unwrap();
        for event in [
            AndroidViewEvent::Touch {
                phase: AndroidTouchPhase::Down,
                pointer: 0,
                x: 20.0,
                y: 20.0,
            },
            AndroidViewEvent::Lifecycle { resumed: false },
            AndroidViewEvent::Touch {
                phase: AndroidTouchPhase::Up,
                pointer: 0,
                x: 20.0,
                y: 20.0,
            },
            AndroidViewEvent::Lifecycle { resumed: true },
        ] {
            host.dispatch(event).unwrap();
        }
        assert_eq!(count.load(Ordering::SeqCst), 0);
        assert!(host
            .dispatch(AndroidViewEvent::Resize {
                width: -1,
                height: 600,
                density: 1.0,
                font_scale: 1.0
            })
            .is_err());
    }

    #[test]
    fn raster_resources_require_receiver_acknowledgement_and_keep_alpha() {
        let mut host = host(Arc::new(AtomicUsize::new(0)));
        let image = crate::ZsImageFrame::from_rgba8(
            crate::ZsImageFrameId::new(42),
            2,
            1,
            vec![255, 0, 0, 128, 0, 255, 0, 255],
        )
        .unwrap();
        host.plan.commands = vec![NativeDrawCommand::Image(
            crate::NativeDrawImageCommand::new(
                image,
                Rect {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 1,
                },
                Rect {
                    x: 4,
                    y: 6,
                    width: 20,
                    height: 10,
                },
            ),
        )];
        let first = host.frame(false, 0, vec![]).unwrap();
        assert_eq!(first.images[0]["argb"][0], 0x80ff0000u32);
        assert_eq!(first.images[0]["argb"][1], 0xff00ff00u32);
        assert_eq!(first.commands[0]["source"]["width"], 2);
        // An ignored frame must not consume the resource transfer.
        assert_eq!(host.frame(false, 0, vec![]).unwrap().images.len(), 1);
        host.set_retained_images(&[crate::ZsImageFrameId::new(42)]);
        assert!(host.frame(false, 0, vec![]).unwrap().images.is_empty());
        host.set_retained_images(&[]);
        assert_eq!(host.frame(false, 0, vec![]).unwrap().images.len(), 1);
        host.set_raster_encoding(AndroidRasterEncoding::ArgbBase64);
        let encoded = host.frame(false, 0, vec![]).unwrap();
        assert!(encoded.images[0]["argb"].is_null());
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.images[0]["argb_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(bytes, vec![128, 255, 0, 0, 255, 0, 255, 0]);
        host.set_retained_images(&[crate::ZsImageFrameId::new(42)]);
        assert!(host.frame(false, 0, vec![]).unwrap().images.is_empty());
    }

    #[cfg(feature = "canvas")]
    mod gestures {
        use super::*;
        use std::sync::Mutex;
        #[derive(Clone, Debug, PartialEq)]
        enum Msg {
            Pointer(crate::ZsCanvasPointerPhase),
            Swipe(crate::ZsSwipeDirection),
        }
        fn setup(density: f32) -> (AndroidViewHost, Arc<Mutex<Vec<Msg>>>) {
            let messages = Arc::new(Mutex::new(Vec::new()));
            let seen = messages.clone();
            let mut host = AndroidViewHost::new(crate::native_window("Gestures").stateful_view(
                (),
                |_| {
                    crate::canvas(crate::ZsCanvasScene::new())
                        .id(crate::WidgetId::new(77))
                        .width(Dp::new(300.0))
                        .height(Dp::new(500.0))
                        .on_canvas_pointer_with(|event| Msg::Pointer(event.phase))
                        .on_canvas_swipe_with(|event| Msg::Swipe(event.direction))
                },
                move |_, message, _| seen.lock().unwrap().push(message),
            ))
            .unwrap();
            host.dispatch(AndroidViewEvent::Resize {
                width: (300.0 * density) as i32,
                height: (500.0 * density) as i32,
                density,
                font_scale: 1.0,
            })
            .unwrap();
            host.set_interactions(regions(1)).unwrap();
            (host, messages)
        }
        fn regions(context: u64) -> AndroidInteractions {
            AndroidInteractions {
                context,
                scrolls: vec![],
                swipes: vec![AndroidSwipeArea::new(
                    crate::WidgetId::new(77),
                    AndroidTouchRect::new(
                        Dp::new(0.0),
                        Dp::new(0.0),
                        Dp::new(300.0),
                        Dp::new(500.0),
                    ),
                )],
            }
        }
        fn touch(host: &mut AndroidViewHost, phase: AndroidTouchPhase, x: f32, y: f32) {
            let d = host.density;
            host.dispatch(AndroidViewEvent::Touch {
                phase,
                pointer: 0,
                x: x * d,
                y: y * d,
            })
            .unwrap();
        }
        #[test]
        fn horizontal_swipes_emit_typed_direction_and_cancel_click_at_all_densities() {
            for density in [1.0, 1.5, 2.5, 3.0] {
                let (mut host, seen) = setup(density);
                touch(&mut host, AndroidTouchPhase::Down, 30.0, 100.0);
                touch(&mut host, AndroidTouchPhase::Up, 230.0, 102.0);
                touch(&mut host, AndroidTouchPhase::Down, 230.0, 100.0);
                touch(&mut host, AndroidTouchPhase::Up, 30.0, 102.0);
                let events = seen.lock().unwrap();
                assert_eq!(
                    events.iter().filter(|m| matches!(m, Msg::Swipe(_))).count(),
                    2
                );
                assert!(events.contains(&Msg::Swipe(crate::ZsSwipeDirection::Right)));
                assert!(events.contains(&Msg::Swipe(crate::ZsSwipeDirection::Left)));
                assert!(!events.contains(&Msg::Pointer(crate::ZsCanvasPointerPhase::Released)));
            }
        }
        #[test]
        fn short_diagonal_and_outside_drags_do_not_swipe_or_click() {
            for (x, y) in [(55.0, 102.0), (240.0, 420.0), (350.0, 100.0)] {
                let (mut host, seen) = setup(2.0);
                touch(&mut host, AndroidTouchPhase::Down, 30.0, 100.0);
                touch(&mut host, AndroidTouchPhase::Up, x, y);
                let events = seen.lock().unwrap();
                assert!(!events.iter().any(|m| matches!(
                    m,
                    Msg::Swipe(_) | Msg::Pointer(crate::ZsCanvasPointerPhase::Released)
                )));
            }
        }
        #[test]
        fn exclusions_keep_pointer_drag_controls_operable() {
            let (mut host, seen) = setup(2.0);
            let mut config = regions(1);
            config.swipes[0].exclusions.push(AndroidTouchRect::new(
                Dp::new(0.0),
                Dp::new(300.0),
                Dp::new(300.0),
                Dp::new(50.0),
            ));
            host.set_interactions(config).unwrap();
            touch(&mut host, AndroidTouchPhase::Down, 30.0, 320.0);
            touch(&mut host, AndroidTouchPhase::Move, 130.0, 320.0);
            touch(&mut host, AndroidTouchPhase::Up, 230.0, 320.0);
            let events = seen.lock().unwrap();
            assert!(events.contains(&Msg::Pointer(crate::ZsCanvasPointerPhase::Released)));
            assert!(!events.iter().any(|m| matches!(m, Msg::Swipe(_))));
        }
        #[test]
        fn changed_page_and_lifecycle_cancel_pending_gestures() {
            let (mut host, seen) = setup(1.0);
            touch(&mut host, AndroidTouchPhase::Down, 30.0, 100.0);
            host.set_interactions(regions(2)).unwrap();
            touch(&mut host, AndroidTouchPhase::Up, 230.0, 100.0);
            touch(&mut host, AndroidTouchPhase::Down, 30.0, 100.0);
            host.dispatch(AndroidViewEvent::Lifecycle { resumed: false })
                .unwrap();
            host.dispatch(AndroidViewEvent::Lifecycle { resumed: true })
                .unwrap();
            touch(&mut host, AndroidTouchPhase::Up, 230.0, 100.0);
            assert!(!seen.lock().unwrap().iter().any(|m| matches!(
                m,
                Msg::Swipe(_) | Msg::Pointer(crate::ZsCanvasPointerPhase::Released)
            )));
        }
        #[test]
        fn wrong_pointer_release_does_not_finish_a_swipe() {
            let (mut host, seen) = setup(1.0);
            touch(&mut host, AndroidTouchPhase::Down, 30.0, 100.0);
            host.dispatch(AndroidViewEvent::Touch {
                phase: AndroidTouchPhase::Up,
                pointer: 1,
                x: 230.0,
                y: 100.0,
            })
            .unwrap();
            assert!(!seen
                .lock()
                .unwrap()
                .iter()
                .any(|m| matches!(m, Msg::Swipe(_))));
            touch(&mut host, AndroidTouchPhase::Cancel, 230.0, 100.0);
            touch(&mut host, AndroidTouchPhase::Up, 230.0, 100.0);
            assert!(!seen
                .lock()
                .unwrap()
                .iter()
                .any(|m| matches!(m, Msg::Swipe(_))));
        }
    }
}
