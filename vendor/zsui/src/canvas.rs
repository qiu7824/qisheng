use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::{
    Color, ColorRole, Dp, Dpi, NativeDrawCommand, NativeDrawFill, NativeDrawIconCommand,
    NativeDrawImageCommand, NativeDrawPlan, NativeIconColorMode, NativeImageInterpolation, Rect,
    SemanticTextStyle, WidgetId, ZsIcon, ZsImageFrame, ZsPointerButton, ZsPointerModifiers,
};

/// Lifecycle phase for a Canvas pointer capture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ZsCanvasPointerPhase {
    Pressed,
    Moved,
    Released,
    Cancelled,
}

/// A typed pointer event expressed in the Canvas' local DP coordinate space.
///
/// Drag positions remain unbounded after capture; use `inside` to distinguish
/// movement inside the final Canvas bounds from movement outside them.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZsCanvasPointerEvent {
    pub widget: WidgetId,
    pub phase: ZsCanvasPointerPhase,
    pub position: ZsCanvasPoint,
    pub button: ZsPointerButton,
    pub modifiers: ZsPointerModifiers,
    pub inside: bool,
    /// The topmost scene hover region under `position`, filled in when the
    /// event reaches the Canvas, so custom-drawn controls need no hit test.
    #[serde(default)]
    pub region: Option<u64>,
}

impl ZsCanvasPointerEvent {
    pub const fn new(
        widget: WidgetId,
        phase: ZsCanvasPointerPhase,
        position: ZsCanvasPoint,
        button: ZsPointerButton,
        modifiers: ZsPointerModifiers,
        inside: bool,
    ) -> Self {
        Self {
            widget,
            phase,
            position,
            button,
            modifiers,
            inside,
            region: None,
        }
    }

    pub const fn with_region(mut self, region: Option<u64>) -> Self {
        self.region = region;
        self
    }
}

/// A completed horizontal swipe, independent of a product's navigation rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ZsSwipeDirection {
    Left,
    Right,
}

/// A typed Canvas gesture in local DP coordinates. Recognition and cancellation
/// belong to the host; applications map the direction to their own actions.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZsCanvasSwipeEvent {
    pub widget: WidgetId,
    pub direction: ZsSwipeDirection,
    pub start: ZsCanvasPoint,
    pub end: ZsCanvasPoint,
}

/// A point in a Canvas' local device-independent coordinate system.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZsCanvasPoint {
    pub x: Dp,
    pub y: Dp,
}

impl ZsCanvasPoint {
    pub const fn new(x: Dp, y: Dp) -> Self {
        Self { x, y }
    }
}

/// A rectangle in a Canvas' local device-independent coordinate system.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZsCanvasRect {
    pub x: Dp,
    pub y: Dp,
    pub width: Dp,
    pub height: Dp,
}

impl ZsCanvasRect {
    pub const fn new(x: Dp, y: Dp, width: Dp, height: Dp) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
}

/// Backend-neutral Canvas primitives.
///
/// Coordinates and widths are declared in [`Dp`], while colors and text use
/// semantic roles. Native backends receive the same translated draw plan and
/// retain ownership of rasterization and system typography.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ZsCanvasPrimitive {
    FillRect {
        rect: ZsCanvasRect,
        fill: NativeDrawFill,
    },
    StrokeRect {
        rect: ZsCanvasRect,
        stroke: NativeDrawFill,
        width: Dp,
    },
    StrokeArc {
        rect: ZsCanvasRect,
        stroke: NativeDrawFill,
        width: Dp,
        start_degrees: i16,
        sweep_degrees: i16,
    },
    FillTriangle {
        points: [ZsCanvasPoint; 3],
        fill: NativeDrawFill,
    },
    RoundRect {
        rect: ZsCanvasRect,
        fill: NativeDrawFill,
        stroke: Option<NativeDrawFill>,
        radius: Dp,
    },
    RoundFill {
        rect: ZsCanvasRect,
        fill: NativeDrawFill,
        radius: Dp,
    },
    Text {
        text: String,
        rect: ZsCanvasRect,
        style: SemanticTextStyle,
    },
    Icon {
        icon: ZsIcon,
        rect: ZsCanvasRect,
        color: ColorRole,
    },
    /// Text with an optional brand color and explicit size, for example on
    /// a dark navigation rail, an avatar or an icon-font glyph. High-contrast
    /// mode falls back to `style.color`.
    TextRun {
        text: String,
        rect: ZsCanvasRect,
        style: SemanticTextStyle,
        color: Option<Color>,
        size: Option<Dp>,
        /// A font family that replaces the role's family when installed.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        family: Option<String>,
    },
    /// A theme-aware icon tinted with an explicit brand color. High-contrast
    /// mode falls back to `fallback`.
    ColoredIcon {
        icon: ZsIcon,
        rect: ZsCanvasRect,
        color: Color,
        fallback: ColorRole,
    },
    /// A bitmap such as a system file icon or a thumbnail. A frame whose
    /// pixel size equals `rect` at the Canvas scale factor is drawn 1:1;
    /// other sizes are resampled smoothly.
    Image {
        frame: ZsImageFrame,
        rect: ZsCanvasRect,
    },
    /// Restricts the following primitives to `rect` (intersected with any
    /// enclosing clip and the Canvas bounds) until the matching
    /// [`ZsCanvasPrimitive::PopClip`], for example to reveal part of a line
    /// of text. Unbalanced clips are closed at the end of the scene.
    PushClip { rect: ZsCanvasRect },
    /// Ends the innermost [`ZsCanvasPrimitive::PushClip`]; ignored when no
    /// clip is open.
    PopClip,
    /// An antialiased line through `points` with round joins and caps, for
    /// example a divider at an angle, a sparkline or a waveform. `closed`
    /// joins the last point back to the first.
    StrokePolyline {
        points: Vec<ZsCanvasPoint>,
        stroke: NativeDrawFill,
        width: Dp,
        #[serde(default)]
        closed: bool,
    },
    /// An antialiased polygon with the even-odd fill rule.
    FillPolygon {
        points: Vec<ZsCanvasPoint>,
        fill: NativeDrawFill,
    },
    /// A two-stop linear gradient filling `rect` with optional round
    /// corners. High-contrast mode draws `start` as a solid fill.
    FillGradient {
        rect: ZsCanvasRect,
        start: NativeDrawFill,
        end: NativeDrawFill,
        direction: crate::NativeGradientDirection,
        radius: Dp,
    },
}

impl ZsCanvasPrimitive {
    pub fn fill_rect(rect: ZsCanvasRect, fill: NativeDrawFill) -> Self {
        Self::FillRect { rect, fill }
    }

    pub fn round_fill(rect: ZsCanvasRect, fill: NativeDrawFill, radius: Dp) -> Self {
        Self::RoundFill { rect, fill, radius }
    }

    pub fn text(text: impl Into<String>, rect: ZsCanvasRect, style: SemanticTextStyle) -> Self {
        Self::Text {
            text: text.into(),
            rect,
            style,
        }
    }

    pub const fn icon(icon: ZsIcon, rect: ZsCanvasRect, color: ColorRole) -> Self {
        Self::Icon { icon, rect, color }
    }

    /// Text drawn in `color`; `style.color` remains the high-contrast fallback.
    pub fn colored_text(
        text: impl Into<String>,
        rect: ZsCanvasRect,
        style: SemanticTextStyle,
        color: Color,
    ) -> Self {
        Self::text_run(text, rect, style, Some(color), None)
    }

    /// Text with an optional explicit color and font size in DP.
    pub fn text_run(
        text: impl Into<String>,
        rect: ZsCanvasRect,
        style: SemanticTextStyle,
        color: Option<Color>,
        size: Option<Dp>,
    ) -> Self {
        Self::TextRun {
            text: text.into(),
            rect,
            style,
            color,
            size,
            family: None,
        }
    }

    /// Draws a text run in `family` instead of the role's family, for
    /// example a display face or one with tabular figures. Other
    /// primitives are returned unchanged.
    pub fn with_font_family(mut self, name: impl Into<String>) -> Self {
        if let Self::TextRun { family, .. } = &mut self {
            let name = name.into();
            *family = (!name.trim().is_empty()).then_some(name);
        }
        self
    }

    /// One glyph of the platform icon font (Segoe Fluent Icons or Segoe MDL2
    /// Assets on Windows) centered in `rect` and sized to its shorter side.
    pub fn glyph(glyph: char, rect: ZsCanvasRect, color: Color, fallback: ColorRole) -> Self {
        let mut style = SemanticTextStyle::for_role(crate::TextRole::Icon);
        style.color = fallback;
        style.horizontal_align = crate::HorizontalAlign::Center;
        style.ellipsis = false;
        Self::text_run(
            glyph.to_string(),
            rect,
            style,
            Some(color),
            Some(Dp::new(rect.width.0.min(rect.height.0).max(1.0))),
        )
    }

    /// An icon tinted with `color`; `fallback` is used in high contrast.
    pub const fn colored_icon(
        icon: ZsIcon,
        rect: ZsCanvasRect,
        color: Color,
        fallback: ColorRole,
    ) -> Self {
        Self::ColoredIcon {
            icon,
            rect,
            color,
            fallback,
        }
    }

    /// A bitmap drawn into `rect`.
    pub fn image(frame: ZsImageFrame, rect: ZsCanvasRect) -> Self {
        Self::Image { frame, rect }
    }

    /// Starts clipping the following primitives to `rect`.
    pub const fn push_clip(rect: ZsCanvasRect) -> Self {
        Self::PushClip { rect }
    }

    /// Ends the innermost clip started with [`Self::push_clip`].
    pub const fn pop_clip() -> Self {
        Self::PopClip
    }

    /// A straight antialiased line from `from` to `to`.
    pub fn line(from: ZsCanvasPoint, to: ZsCanvasPoint, stroke: NativeDrawFill, width: Dp) -> Self {
        Self::polyline(vec![from, to], stroke, width)
    }

    /// An open antialiased line through `points`.
    pub fn polyline(points: Vec<ZsCanvasPoint>, stroke: NativeDrawFill, width: Dp) -> Self {
        Self::StrokePolyline {
            points,
            stroke,
            width,
            closed: false,
        }
    }

    /// The outline of the polygon through `points`.
    pub fn polygon_outline(points: Vec<ZsCanvasPoint>, stroke: NativeDrawFill, width: Dp) -> Self {
        Self::StrokePolyline {
            points,
            stroke,
            width,
            closed: true,
        }
    }

    /// A filled antialiased polygon through `points`.
    pub fn polygon(points: Vec<ZsCanvasPoint>, fill: NativeDrawFill) -> Self {
        Self::FillPolygon { points, fill }
    }

    /// A gradient from `start` at the top of `rect` to `end` at its bottom.
    pub const fn vertical_gradient(
        rect: ZsCanvasRect,
        start: NativeDrawFill,
        end: NativeDrawFill,
        radius: Dp,
    ) -> Self {
        Self::FillGradient {
            rect,
            start,
            end,
            direction: crate::NativeGradientDirection::Vertical,
            radius,
        }
    }

    /// A gradient from `start` at the left of `rect` to `end` at its right.
    pub const fn horizontal_gradient(
        rect: ZsCanvasRect,
        start: NativeDrawFill,
        end: NativeDrawFill,
        radius: Dp,
    ) -> Self {
        Self::FillGradient {
            rect,
            start,
            end,
            direction: crate::NativeGradientDirection::Horizontal,
            radius,
        }
    }
}

/// Immutable custom-drawing content retained by a Canvas View node.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ZsCanvasScene {
    primitives: Vec<ZsCanvasPrimitive>,
    /// Natural content height requested by a size-aware Canvas. Layout uses it
    /// when the node declares no height, for example inside a Scroll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    extent_height: Option<Dp>,
    /// Rectangles that report pointer hover through `on_canvas_hover`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    hover_regions: Vec<ZsCanvasHoverRegion>,
    /// Accessible names (and tooltips) of labeled regions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    region_labels: Vec<ZsCanvasRegionLabel>,
}

/// A rectangle of a Canvas scene that reports pointer hover.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZsCanvasHoverRegion {
    pub id: u64,
    pub rect: ZsCanvasRect,
    /// The pointer cursor over this region; `None` uses the Canvas cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<crate::ZsCursor>,
}

/// The accessible name of a labeled region. Assistive technology exposes
/// the region as a button with this name, and hovering it shows the name as
/// a tooltip unless the label is marked silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZsCanvasRegionLabel {
    pub id: u64,
    pub label: String,
    /// Show the label as a tooltip on hover.
    #[serde(default = "default_true")]
    pub tooltip: bool,
}

fn default_true() -> bool {
    true
}

/// A region of a Canvas scene described in one place: where it is, the
/// cursor over it, and its accessible label.
#[derive(Debug, Clone, PartialEq)]
pub struct ZsCanvasRegion {
    id: u64,
    rect: ZsCanvasRect,
    cursor: Option<crate::ZsCursor>,
    label: Option<String>,
    tooltip: bool,
}

impl ZsCanvasRegion {
    pub fn new(id: u64, rect: ZsCanvasRect) -> Self {
        Self {
            id,
            rect,
            cursor: None,
            label: None,
            tooltip: true,
        }
    }

    /// The pointer cursor over this region.
    pub fn cursor(mut self, cursor: crate::ZsCursor) -> Self {
        self.cursor = Some(cursor);
        self
    }

    /// The accessible name, also shown as a tooltip on hover.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        let label = label.into();
        self.label = (!label.trim().is_empty()).then_some(label);
        self
    }

    /// Keeps the accessible name but shows no tooltip, for regions whose
    /// visible text already says what they do.
    pub fn without_tooltip(mut self) -> Self {
        self.tooltip = false;
        self
    }
}

/// The synthetic widget id of a labeled Canvas region, used for its
/// tooltip and its accessibility node.
#[cfg(any(test, feature = "tooltip", feature = "accessibility"))]
pub(crate) const fn zs_canvas_region_widget(
    canvas: crate::WidgetId,
    region: u64,
) -> crate::WidgetId {
    let mut hash = canvas.0 ^ 0x6361_6e76_6173_7267;
    hash = hash.rotate_left(29).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ region;
    hash ^= hash >> 31;
    hash = hash.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    hash ^= hash >> 29;
    crate::WidgetId(hash | (1 << 63))
}

/// A mouse wheel turn over a Canvas with `on_canvas_wheel`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZsCanvasWheelEvent {
    /// The topmost hover region under the pointer.
    pub region: Option<u64>,
    /// The pointer position in local DP.
    pub position: ZsCanvasPoint,
    /// Scroll distance in DP; positive moves content up (wheel turned
    /// towards the user), like a Scroll offset change.
    pub delta: Dp,
}

impl ZsCanvasScene {
    pub const fn new() -> Self {
        Self {
            primitives: Vec::new(),
            extent_height: None,
            hover_regions: Vec::new(),
            region_labels: Vec::new(),
        }
    }

    /// Declares the natural height of this scene's content in local DP.
    ///
    /// Only size-aware canvases created with [`crate::canvas_with`] consult it:
    /// layout asks the builder for the scene at the available width and uses
    /// this height when the node has no explicit height.
    pub fn with_extent_height(mut self, height: Dp) -> Self {
        self.extent_height = Some(Dp::new(sanitized_extent(height.0)));
        self
    }

    pub fn set_extent_height(&mut self, height: Dp) {
        self.extent_height = Some(Dp::new(sanitized_extent(height.0)));
    }

    pub fn extent_height(&self) -> Option<Dp> {
        self.extent_height
    }

    /// Declares a hover region, for example one per tab, row or button
    /// drawn in this scene. Later regions sit above earlier ones.
    pub fn with_hover_region(mut self, id: u64, rect: ZsCanvasRect) -> Self {
        self.push_hover_region(id, rect);
        self
    }

    pub fn push_hover_region(&mut self, id: u64, rect: ZsCanvasRect) {
        self.hover_regions.push(ZsCanvasHoverRegion {
            id,
            rect,
            cursor: None,
        });
    }

    /// Declares a region with a cursor and an accessible label. Labeled
    /// regions appear to assistive technology as buttons, show their label
    /// as a tooltip, and can be invoked like a click at their center.
    pub fn push_region(&mut self, region: ZsCanvasRegion) {
        self.hover_regions.push(ZsCanvasHoverRegion {
            id: region.id,
            rect: region.rect,
            cursor: region.cursor,
        });
        if let Some(label) = region.label {
            self.region_labels
                .retain(|existing| existing.id != region.id);
            self.region_labels.push(ZsCanvasRegionLabel {
                id: region.id,
                label,
                tooltip: region.tooltip,
            });
        }
    }

    pub fn with_region(mut self, region: ZsCanvasRegion) -> Self {
        self.push_region(region);
        self
    }

    pub fn region_labels(&self) -> &[ZsCanvasRegionLabel] {
        &self.region_labels
    }

    /// The label of region `id`, if it has one.
    pub fn region_label(&self, id: u64) -> Option<&ZsCanvasRegionLabel> {
        self.region_labels.iter().find(|label| label.id == id)
    }

    /// The topmost region containing `point`, in local DP.
    pub fn region_at(&self, point: ZsCanvasPoint) -> Option<&ZsCanvasHoverRegion> {
        self.hover_regions.iter().rev().find(|region| {
            let rect = region.rect;
            point.x.0 >= rect.x.0
                && point.y.0 >= rect.y.0
                && point.x.0 < rect.x.0 + rect.width.0
                && point.y.0 < rect.y.0 + rect.height.0
        })
    }

    pub fn hover_regions(&self) -> &[ZsCanvasHoverRegion] {
        &self.hover_regions
    }

    /// The topmost hover region containing `point`, in local DP.
    pub fn hover_region_at(&self, point: ZsCanvasPoint) -> Option<u64> {
        self.region_at(point).map(|region| region.id)
    }

    pub fn with(mut self, primitive: ZsCanvasPrimitive) -> Self {
        self.primitives.push(primitive);
        self
    }

    pub fn push(&mut self, primitive: ZsCanvasPrimitive) {
        self.primitives.push(primitive);
    }

    pub fn primitives(&self) -> &[ZsCanvasPrimitive] {
        &self.primitives
    }

    pub fn primitive_count(&self) -> usize {
        self.primitives.len()
    }

    pub fn is_empty(&self) -> bool {
        self.primitives.is_empty()
    }
}

impl FromIterator<ZsCanvasPrimitive> for ZsCanvasScene {
    fn from_iter<T: IntoIterator<Item = ZsCanvasPrimitive>>(iter: T) -> Self {
        Self {
            primitives: iter.into_iter().collect(),
            extent_height: None,
            hover_regions: Vec::new(),
            region_labels: Vec::new(),
        }
    }
}

fn sanitized_extent(value: f32) -> f32 {
    if value.is_finite() {
        value.max(0.0)
    } else {
        0.0
    }
}

/// Final size of a Canvas in its own local DP coordinate space.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ZsCanvasSize {
    pub width: Dp,
    pub height: Dp,
}

impl ZsCanvasSize {
    pub const fn new(width: Dp, height: Dp) -> Self {
        Self { width, height }
    }
}

/// Layout-time context handed to a size-aware Canvas builder.
///
/// It carries the Canvas' final local size and measures text with the same
/// bounded, backend-produced text measurements that ordinary labels use.
/// Measuring with `max_width` equal to the width of the Text primitive that is
/// later drawn guarantees the geometry converges on the native text engine:
/// the first frame may use the framework estimate, every later layout uses the
/// renderer's own measurement.
pub struct ZsCanvasLayoutContext<'a> {
    size: ZsCanvasSize,
    dpi: Dpi,
    typography_scale: f32,
    measurements: Option<&'a crate::view::ViewTextMeasurements>,
    visible: Option<ZsCanvasRect>,
}

impl<'a> ZsCanvasLayoutContext<'a> {
    pub(crate) fn new(
        size: ZsCanvasSize,
        dpi: Dpi,
        typography_scale: f32,
        measurements: Option<&'a crate::view::ViewTextMeasurements>,
    ) -> Self {
        Self {
            size,
            dpi,
            typography_scale,
            measurements,
            visible: None,
        }
    }

    pub(crate) fn with_visible_rect(mut self, visible: Option<ZsCanvasRect>) -> Self {
        self.visible = visible;
        self
    }

    /// A context without native measurements, for previews and tests. Text
    /// measurement falls back to the framework estimate.
    pub fn detached(size: ZsCanvasSize) -> ZsCanvasLayoutContext<'static> {
        ZsCanvasLayoutContext {
            size,
            dpi: Dpi::standard(),
            typography_scale: 1.0,
            measurements: None,
            visible: None,
        }
    }

    /// A detached context whose visible part is `visible`, for tests of
    /// virtualized drawing.
    pub fn detached_with_visible_rect(
        size: ZsCanvasSize,
        visible: ZsCanvasRect,
    ) -> ZsCanvasLayoutContext<'static> {
        Self::detached(size).with_visible_rect(Some(visible))
    }

    /// Final local size of the Canvas. During natural-height measurement the
    /// height is zero and only the width is meaningful.
    pub fn size(&self) -> ZsCanvasSize {
        self.size
    }

    /// The part of the Canvas that can be seen through its enclosing scroll
    /// viewports, in local DP.
    ///
    /// A tall Canvas inside a Scroll uses it to emit primitives and hover
    /// regions only for the rows that are on screen, so a list of thousands
    /// of custom-drawn rows builds a scene the size of one screen. Layout
    /// rebuilds the scene whenever the scroll offset changes. During
    /// natural-height measurement (zero height) the whole Canvas is reported.
    pub fn visible_rect(&self) -> ZsCanvasRect {
        let whole = ZsCanvasRect::new(
            Dp::new(0.0),
            Dp::new(0.0),
            self.size.width,
            self.size.height,
        );
        if self.size.height.0 <= 0.0 {
            return whole;
        }
        self.visible.unwrap_or(whole)
    }

    /// Physical pixels per DP. Multiply a rectangle by it to request a bitmap
    /// that [`ZsCanvasPrimitive::Image`] draws without resampling.
    pub fn scale_factor(&self) -> f32 {
        self.dpi.scale_factor()
    }

    /// Measures `text` in local DP. `max_width` only applies to
    /// [`crate::TextWrap::Word`] styles; other styles measure a single line.
    pub fn measure_text(
        &self,
        text: &str,
        style: SemanticTextStyle,
        max_width: Option<Dp>,
    ) -> ZsCanvasSize {
        self.measure_text_at(text, style, None, max_width)
    }

    /// Measures `text` drawn at an explicit font `size` in DP, matching a
    /// [`ZsCanvasPrimitive::TextRun`] with the same style, size and width.
    /// `None` uses the style role's platform size like [`Self::measure_text`].
    pub fn measure_text_at(
        &self,
        text: &str,
        style: SemanticTextStyle,
        size: Option<Dp>,
        max_width: Option<Dp>,
    ) -> ZsCanvasSize {
        self.measure_text_with(text, style, size, None, max_width)
    }

    /// Measures `text` drawn at an explicit size in an explicit font
    /// family, matching a [`ZsCanvasPrimitive::TextRun`] built with
    /// [`ZsCanvasPrimitive::with_font_family`].
    pub fn measure_text_with(
        &self,
        text: &str,
        style: SemanticTextStyle,
        size: Option<Dp>,
        family: Option<&str>,
        max_width: Option<Dp>,
    ) -> ZsCanvasSize {
        let max_px = max_width
            .filter(|_| style.wrap == crate::TextWrap::Word)
            .map(|width| canvas_non_negative_px(width, self.dpi))
            .unwrap_or(0);
        let font_size = size
            .filter(|size| size.0.is_finite() && size.0 > 0.0)
            .map(|size| crate::ZsFontSize::from_dip(size.0));
        let overrides = crate::ZsTextOverrides {
            size: font_size,
            family: family
                .filter(|family| !family.trim().is_empty())
                .map(crate::ZsFontFamily::new),
        };
        if let Some(measured) = self
            .measurements
            .and_then(|measurements| measurements.measure_sized(text, style, overrides, max_px))
        {
            let scale = self.dpi.scale_factor().max(f32::EPSILON);
            return ZsCanvasSize::new(
                Dp::new(measured.width.max(0) as f32 / scale),
                Dp::new(measured.height.max(0) as f32 / scale),
            );
        }
        self.estimate_text(
            text,
            style,
            font_size.map(crate::ZsFontSize::dip),
            max_width,
        )
    }

    fn estimate_text(
        &self,
        text: &str,
        style: SemanticTextStyle,
        font_size: Option<f32>,
        max_width: Option<Dp>,
    ) -> ZsCanvasSize {
        let mut metrics = style
            .role
            .metrics_for(crate::ZsTypographyPlatformStyle::current());
        if let Some(size) = font_size.filter(|size| *size > 0.0) {
            let ratio = if metrics.size > 0.0 {
                metrics.line_height / metrics.size
            } else {
                1.25
            };
            metrics.size = size;
            metrics.line_height = size * ratio;
        }
        let scale = self.typography_scale.max(0.0);
        // Framework width units: one unit per Latin cell, two per CJK cell.
        let unit = metrics.size * 0.5 * scale;
        let weight = match style.weight {
            crate::TextWeight::Semibold | crate::TextWeight::Bold => 1.08,
            crate::TextWeight::Medium => 1.04,
            crate::TextWeight::Automatic | crate::TextWeight::Regular => 1.0,
        };
        let line_height = metrics.line_height * scale;
        let wrap_width = max_width
            .filter(|_| style.wrap == crate::TextWrap::Word)
            .map(|width| width.0.max(1.0));
        let mut widest = 0.0_f32;
        let mut rows = 0_u32;
        for line in text.split('\n') {
            let width =
                crate::widget_render::zs_estimated_text_width_units(line) as f32 * unit * weight;
            match wrap_width {
                Some(limit) if width > limit => {
                    rows += (width / limit).ceil().max(1.0) as u32;
                    widest = widest.max(limit);
                }
                _ => {
                    rows += 1;
                    widest = widest.max(width);
                }
            }
        }
        ZsCanvasSize::new(
            Dp::new(widest.ceil()),
            Dp::new(line_height * rows.max(1) as f32),
        )
    }
}

type ZsCanvasBuildFn = dyn Fn(&ZsCanvasLayoutContext<'_>) -> ZsCanvasScene + Send + Sync;

/// Rebuilds a Canvas scene from the Canvas' final layout size.
///
/// Layout invokes it once the Canvas has bounds (and once more at the
/// available width when the node needs a natural height), so applications can
/// right-align, wrap and size custom drawing without platform handles.
#[derive(Clone)]
pub struct ZsCanvasBuilder {
    build: Arc<ZsCanvasBuildFn>,
}

impl ZsCanvasBuilder {
    pub fn new(
        build: impl Fn(&ZsCanvasLayoutContext<'_>) -> ZsCanvasScene + Send + Sync + 'static,
    ) -> Self {
        Self {
            build: Arc::new(build),
        }
    }

    pub fn build(&self, context: &ZsCanvasLayoutContext<'_>) -> ZsCanvasScene {
        (self.build)(context)
    }
}

impl fmt::Debug for ZsCanvasBuilder {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ZsCanvasBuilder")
    }
}

/// Converts a local-DP Canvas scene into the shared native draw protocol.
///
/// The generated clip is always balanced and confines every primitive to the
/// Canvas' final layout bounds on Win32, AppKit and Linux renderers.
pub fn zs_canvas_native_draw_plan(bounds: Rect, scene: &ZsCanvasScene, dpi: Dpi) -> NativeDrawPlan {
    let mut plan = NativeDrawPlan::default();
    plan.push(NativeDrawCommand::PushClip { rect: bounds });
    let mut open_clips = 0usize;
    for primitive in scene.primitives() {
        match primitive {
            ZsCanvasPrimitive::PushClip { .. } => open_clips += 1,
            ZsCanvasPrimitive::PopClip if open_clips == 0 => continue,
            ZsCanvasPrimitive::PopClip => open_clips -= 1,
            _ => {}
        }
        plan.push(canvas_primitive_to_native(bounds, primitive, dpi));
    }
    for _ in 0..open_clips {
        plan.push(NativeDrawCommand::PopClip);
    }
    plan.push(NativeDrawCommand::PopClip);
    plan
}

fn canvas_primitive_to_native(
    bounds: Rect,
    primitive: &ZsCanvasPrimitive,
    dpi: Dpi,
) -> NativeDrawCommand {
    match primitive {
        ZsCanvasPrimitive::FillRect { rect, fill } => NativeDrawCommand::FillRect {
            rect: canvas_rect_to_native(bounds, *rect, dpi),
            fill: *fill,
        },
        ZsCanvasPrimitive::StrokeRect {
            rect,
            stroke,
            width,
        } => NativeDrawCommand::StrokeRect {
            rect: canvas_rect_to_native(bounds, *rect, dpi),
            stroke: *stroke,
            width: canvas_non_negative_px(*width, dpi).max(1),
        },
        ZsCanvasPrimitive::StrokeArc {
            rect,
            stroke,
            width,
            start_degrees,
            sweep_degrees,
        } => NativeDrawCommand::StrokeArc {
            rect: canvas_rect_to_native(bounds, *rect, dpi),
            stroke: *stroke,
            width: canvas_non_negative_px(*width, dpi).max(1),
            start_degrees: *start_degrees,
            sweep_degrees: *sweep_degrees,
        },
        ZsCanvasPrimitive::FillTriangle { points, fill } => NativeDrawCommand::FillTriangle {
            points: points.map(|point| canvas_point_to_native(bounds, point, dpi)),
            fill: *fill,
        },
        ZsCanvasPrimitive::RoundRect {
            rect,
            fill,
            stroke,
            radius,
        } => NativeDrawCommand::RoundRect {
            rect: canvas_rect_to_native(bounds, *rect, dpi),
            fill: *fill,
            stroke: *stroke,
            radius: canvas_non_negative_px(*radius, dpi),
        },
        ZsCanvasPrimitive::RoundFill { rect, fill, radius } => NativeDrawCommand::RoundFill {
            rect: canvas_rect_to_native(bounds, *rect, dpi),
            fill: *fill,
            radius: canvas_non_negative_px(*radius, dpi),
        },
        ZsCanvasPrimitive::Text { text, rect, style } => {
            NativeDrawCommand::Text(crate::NativeDrawTextCommand::new(
                text,
                canvas_rect_to_native(bounds, *rect, dpi),
                *style,
            ))
        }
        ZsCanvasPrimitive::Icon { icon, rect, color } => NativeDrawCommand::Icon(
            NativeDrawIconCommand::new(
                *icon,
                canvas_rect_to_native(bounds, *rect, dpi),
                NativeIconColorMode::ThemeAware,
            )
            .with_color(*color),
        ),
        ZsCanvasPrimitive::TextRun {
            text,
            rect,
            style,
            color,
            size,
            family,
        } => {
            let mut command = crate::NativeDrawTextCommand::new(
                text,
                canvas_rect_to_native(bounds, *rect, dpi),
                *style,
            );
            command.color = *color;
            command.size = size.map(|size| crate::ZsFontSize::from_dip(size.0));
            command.family = family.clone();
            NativeDrawCommand::Text(command)
        }
        ZsCanvasPrimitive::ColoredIcon {
            icon,
            rect,
            color,
            fallback,
        } => NativeDrawCommand::Icon(
            NativeDrawIconCommand::new(
                *icon,
                canvas_rect_to_native(bounds, *rect, dpi),
                NativeIconColorMode::ThemeAware,
            )
            .with_color(*fallback)
            .with_custom_color(*color),
        ),
        ZsCanvasPrimitive::Image { frame, rect } => {
            let bounds = canvas_rect_to_native(bounds, *rect, dpi);
            let source = Rect {
                x: 0,
                y: 0,
                width: i32::try_from(frame.width()).unwrap_or(i32::MAX),
                height: i32::try_from(frame.height()).unwrap_or(i32::MAX),
            };
            let interpolation = if source.width == bounds.width && source.height == bounds.height {
                NativeImageInterpolation::Nearest
            } else {
                NativeImageInterpolation::Smooth
            };
            NativeDrawCommand::Image(
                NativeDrawImageCommand::new(frame.clone(), source, bounds)
                    .interpolation(interpolation),
            )
        }
        ZsCanvasPrimitive::PushClip { rect } => NativeDrawCommand::PushClip {
            rect: canvas_rect_to_native(bounds, *rect, dpi),
        },
        ZsCanvasPrimitive::PopClip => NativeDrawCommand::PopClip,
        ZsCanvasPrimitive::StrokePolyline {
            points,
            stroke,
            width,
            closed,
        } => NativeDrawCommand::StrokePolyline {
            points: points
                .iter()
                .map(|point| canvas_point_to_native(bounds, *point, dpi))
                .collect(),
            stroke: *stroke,
            width: canvas_non_negative_px(*width, dpi).max(1),
            closed: *closed,
        },
        ZsCanvasPrimitive::FillPolygon { points, fill } => NativeDrawCommand::FillPolygon {
            points: points
                .iter()
                .map(|point| canvas_point_to_native(bounds, *point, dpi))
                .collect(),
            fill: *fill,
        },
        ZsCanvasPrimitive::FillGradient {
            rect,
            start,
            end,
            direction,
            radius,
        } => NativeDrawCommand::FillGradient {
            rect: canvas_rect_to_native(bounds, *rect, dpi),
            start: *start,
            end: *end,
            direction: *direction,
            radius: canvas_non_negative_px(*radius, dpi),
        },
    }
}

fn canvas_point_to_native(bounds: Rect, point: ZsCanvasPoint, dpi: Dpi) -> crate::Point {
    crate::Point {
        x: bounds.x.saturating_add(canvas_signed_px(point.x, dpi)),
        y: bounds.y.saturating_add(canvas_signed_px(point.y, dpi)),
    }
}

fn canvas_rect_to_native(bounds: Rect, rect: ZsCanvasRect, dpi: Dpi) -> Rect {
    Rect {
        x: bounds.x.saturating_add(canvas_signed_px(rect.x, dpi)),
        y: bounds.y.saturating_add(canvas_signed_px(rect.y, dpi)),
        width: canvas_non_negative_px(rect.width, dpi),
        height: canvas_non_negative_px(rect.height, dpi),
    }
}

fn canvas_signed_px(value: Dp, dpi: Dpi) -> i32 {
    if value.0.is_finite() {
        value.to_px(dpi).round_i32()
    } else {
        0
    }
}

fn canvas_non_negative_px(value: Dp, dpi: Dpi) -> i32 {
    canvas_signed_px(value, dpi).max(0)
}

pub(crate) fn zs_canvas_pointer_event(
    widget: WidgetId,
    bounds: Rect,
    point: crate::Point,
    dpi: Dpi,
    phase: ZsCanvasPointerPhase,
    button: ZsPointerButton,
    modifiers: ZsPointerModifiers,
) -> ZsCanvasPointerEvent {
    let scale = dpi.scale_factor();
    ZsCanvasPointerEvent::new(
        widget,
        phase,
        ZsCanvasPoint::new(
            Dp::new((point.x.saturating_sub(bounds.x)) as f32 / scale),
            Dp::new((point.y.saturating_sub(bounds.y)) as f32 / scale),
        ),
        button,
        modifiers,
        bounds.contains(point),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_polygons_and_gradients_scale_into_canvas_bounds() {
        let ink = NativeDrawFill::Role(ColorRole::Accent);
        let point = |x, y| ZsCanvasPoint::new(Dp::new(x), Dp::new(y));
        let scene = ZsCanvasScene::new()
            .with(ZsCanvasPrimitive::line(
                point(0.0, 1.0),
                point(10.0, 1.0),
                ink,
                Dp::new(0.2),
            ))
            .with(ZsCanvasPrimitive::polygon(
                vec![point(0.0, 0.0), point(4.0, 0.0), point(2.0, 3.0)],
                ink,
            ))
            .with(ZsCanvasPrimitive::vertical_gradient(
                ZsCanvasRect::new(Dp::new(1.0), Dp::new(2.0), Dp::new(10.0), Dp::new(5.0)),
                ink,
                NativeDrawFill::Role(ColorRole::Surface),
                Dp::new(3.0),
            ));
        let bounds = Rect {
            x: 100,
            y: 40,
            width: 200,
            height: 100,
        };
        let plan = zs_canvas_native_draw_plan(bounds, &scene, Dpi::new(192.0));
        assert_eq!(
            plan.commands[1],
            NativeDrawCommand::StrokePolyline {
                points: vec![
                    crate::Point { x: 100, y: 42 },
                    crate::Point { x: 120, y: 42 }
                ],
                stroke: ink,
                width: 1,
                closed: false,
            }
        );
        assert_eq!(
            plan.commands[2],
            NativeDrawCommand::FillPolygon {
                points: vec![
                    crate::Point { x: 100, y: 40 },
                    crate::Point { x: 108, y: 40 },
                    crate::Point { x: 104, y: 46 }
                ],
                fill: ink,
            }
        );
        assert_eq!(
            plan.commands[3],
            NativeDrawCommand::FillGradient {
                rect: Rect {
                    x: 102,
                    y: 44,
                    width: 20,
                    height: 10,
                },
                start: ink,
                end: NativeDrawFill::Role(ColorRole::Surface),
                direction: crate::NativeGradientDirection::Vertical,
                radius: 6,
            }
        );
    }

    #[test]
    fn canvas_images_draw_one_to_one_at_their_pixel_size() {
        let frame = ZsImageFrame::from_rgba8(
            crate::ZsImageFrameId::new(7),
            32,
            32,
            vec![255; 32 * 32 * 4],
        )
        .expect("frame");
        let scene = ZsCanvasScene::new()
            .with(ZsCanvasPrimitive::image(
                frame.clone(),
                ZsCanvasRect::new(Dp::new(4.0), Dp::new(2.0), Dp::new(16.0), Dp::new(16.0)),
            ))
            .with(ZsCanvasPrimitive::image(
                frame,
                ZsCanvasRect::new(Dp::new(30.0), Dp::new(2.0), Dp::new(24.0), Dp::new(24.0)),
            ));
        let bounds = Rect {
            x: 100,
            y: 40,
            width: 200,
            height: 100,
        };
        let plan = zs_canvas_native_draw_plan(bounds, &scene, Dpi::new(192.0));
        let images = plan
            .commands
            .iter()
            .filter_map(|command| match command {
                NativeDrawCommand::Image(image) => Some(image),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(images.len(), 2);
        assert_eq!(
            images[0].bounds,
            Rect {
                x: 108,
                y: 44,
                width: 32,
                height: 32,
            }
        );
        assert_eq!(images[0].source.width, 32);
        assert_eq!(images[0].interpolation, NativeImageInterpolation::Nearest);
        assert_eq!(images[1].bounds.width, 48);
        assert_eq!(images[1].interpolation, NativeImageInterpolation::Smooth);
        assert_eq!(
            ZsCanvasLayoutContext::detached(ZsCanvasSize::new(Dp::new(1.0), Dp::new(1.0)))
                .scale_factor(),
            1.0
        );
    }

    #[test]
    fn canvas_clip_primitives_stay_balanced() {
        let scene = ZsCanvasScene::new()
            .with(ZsCanvasPrimitive::pop_clip())
            .with(ZsCanvasPrimitive::push_clip(ZsCanvasRect::new(
                Dp::new(2.0),
                Dp::new(2.0),
                Dp::new(10.0),
                Dp::new(10.0),
            )))
            .with(ZsCanvasPrimitive::fill_rect(
                ZsCanvasRect::new(Dp::new(0.0), Dp::new(0.0), Dp::new(40.0), Dp::new(40.0)),
                NativeDrawFill::role(ColorRole::Accent),
            ));
        let plan = zs_canvas_native_draw_plan(
            Rect {
                x: 10,
                y: 10,
                width: 100,
                height: 50,
            },
            &scene,
            Dpi::standard(),
        );
        let pushes = plan
            .commands
            .iter()
            .filter(|c| matches!(c, NativeDrawCommand::PushClip { .. }))
            .count();
        let pops = plan
            .commands
            .iter()
            .filter(|c| matches!(c, NativeDrawCommand::PopClip))
            .count();
        // The stray leading pop is dropped and the open clip is closed.
        assert_eq!(pushes, 2);
        assert_eq!(pops, 2);
        assert_eq!(
            plan.commands[1],
            NativeDrawCommand::PushClip {
                rect: Rect {
                    x: 12,
                    y: 12,
                    width: 10,
                    height: 10,
                },
            }
        );
    }

    #[test]
    fn canvas_measures_text_at_an_explicit_size() {
        let cx = ZsCanvasLayoutContext::detached(ZsCanvasSize::new(Dp::new(400.0), Dp::new(100.0)));
        let style = SemanticTextStyle::body();
        let base = cx.measure_text("abcd", style, None);
        let large = cx.measure_text_at("abcd", style, Some(Dp::new(48.0)), None);
        assert!(large.width.0 > base.width.0 * 2.0);
        assert!(large.height.0 > base.height.0 * 2.0);
    }

    #[test]
    fn canvas_translates_local_dp_and_balances_clip() {
        let scene = ZsCanvasScene::new()
            .with(ZsCanvasPrimitive::fill_rect(
                ZsCanvasRect::new(Dp::new(4.0), Dp::new(6.0), Dp::new(20.0), Dp::new(10.0)),
                NativeDrawFill::role(ColorRole::Accent),
            ))
            .with(ZsCanvasPrimitive::text(
                "Canvas",
                ZsCanvasRect::new(Dp::new(8.0), Dp::new(20.0), Dp::new(80.0), Dp::new(24.0)),
                SemanticTextStyle::body(),
            ));
        let plan = zs_canvas_native_draw_plan(
            Rect {
                x: 100,
                y: 40,
                width: 200,
                height: 100,
            },
            &scene,
            Dpi::new(192.0),
        );

        assert_eq!(plan.commands.len(), 4);
        assert_eq!(
            plan.commands.first(),
            Some(&NativeDrawCommand::PushClip {
                rect: Rect {
                    x: 100,
                    y: 40,
                    width: 200,
                    height: 100,
                },
            })
        );
        assert!(matches!(
            &plan.commands[1],
            NativeDrawCommand::FillRect {
                rect: Rect {
                    x: 108,
                    y: 52,
                    width: 40,
                    height: 20,
                },
                ..
            }
        ));
        assert_eq!(plan.commands.last(), Some(&NativeDrawCommand::PopClip));
    }

    #[test]
    fn canvas_sanitizes_non_finite_and_negative_extents() {
        let scene = ZsCanvasScene::new().with(ZsCanvasPrimitive::RoundFill {
            rect: ZsCanvasRect::new(
                Dp::new(f32::NAN),
                Dp::new(f32::INFINITY),
                Dp::new(-20.0),
                Dp::new(12.0),
            ),
            fill: NativeDrawFill::role(ColorRole::Control),
            radius: Dp::new(-4.0),
        });
        let plan = zs_canvas_native_draw_plan(
            Rect {
                x: 5,
                y: 7,
                width: 80,
                height: 40,
            },
            &scene,
            Dpi::standard(),
        );

        assert!(matches!(
            &plan.commands[1],
            NativeDrawCommand::RoundFill {
                rect: Rect {
                    x: 5,
                    y: 7,
                    width: 0,
                    height: 12,
                },
                radius: 0,
                ..
            }
        ));
    }

    #[test]
    fn canvas_pointer_events_use_local_dp_and_preserve_outside_drag_positions() {
        let event = zs_canvas_pointer_event(
            WidgetId::new(7),
            Rect {
                x: 100,
                y: 40,
                width: 200,
                height: 100,
            },
            crate::Point { x: 80, y: 260 },
            Dpi::new(192.0),
            ZsCanvasPointerPhase::Moved,
            ZsPointerButton::Secondary,
            ZsPointerModifiers::new(true, false, true, false),
        );

        assert_eq!(
            event.position,
            ZsCanvasPoint::new(Dp::new(-10.0), Dp::new(110.0))
        );
        assert!(!event.inside);
        assert_eq!(event.button, ZsPointerButton::Secondary);
        assert!(event.modifiers.shift);
        assert!(event.modifiers.alt);
    }
}
