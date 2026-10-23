//! Small painter over a ZSUI canvas scene, plus the shared controls every
//! page draws with it: buttons, chips, segmented switches and section
//! headings. Coordinates are local DP.

use crate::theme::{NUM_FONT, Palette, mix, with_alpha};
use zsui::{
    Color, Dp, HorizontalAlign, NativeDrawFill, SemanticTextStyle, TextRole, TextWeight, TextWrap,
    VerticalAlign, ZsCanvasLayoutContext, ZsCanvasPrimitive as Prim, ZsCanvasRect, ZsCanvasRegion,
    ZsCanvasScene, ZsCursor, ZsImageFrame,
};

pub fn rect(x: f32, y: f32, w: f32, h: f32) -> ZsCanvasRect {
    ZsCanvasRect::new(
        Dp::new(x),
        Dp::new(y),
        Dp::new(w.max(0.0)),
        Dp::new(h.max(0.0)),
    )
}

/// Builds hover-region ids: a kind in the high half, an index in the low half.
pub const fn rid(kind: u32, index: u32) -> u64 {
    ((kind as u64) << 32) | index as u64
}

pub const fn rsplit(id: u64) -> (u32, u32) {
    ((id >> 32) as u32, (id & 0xffff_ffff) as u32)
}

pub fn line_style(size: f32, align: HorizontalAlign, weight: TextWeight) -> SemanticTextStyle {
    let role = if size <= 12.5 {
        TextRole::Caption
    } else if size >= 20.0 {
        TextRole::Title
    } else {
        TextRole::Body
    };
    let mut style = SemanticTextStyle::for_role(role);
    style.horizontal_align = align;
    style.vertical_align = VerticalAlign::Center;
    style.weight = weight;
    style.wrap = TextWrap::NoWrap;
    style.ellipsis = true;
    style
}

pub fn para_style(size: f32, align: HorizontalAlign, weight: TextWeight) -> SemanticTextStyle {
    let mut style = line_style(size, align, weight);
    style.vertical_align = VerticalAlign::Start;
    style.wrap = TextWrap::Word;
    style.ellipsis = false;
    style
}

pub struct P {
    pub scene: ZsCanvasScene,
    pub pal: Palette,
    regions: Vec<Region>,
}

/// An interactive area, added to the scene by [`P::finish`] in the order
/// declared so later regions stay on top.
struct Region {
    id: u64,
    rect: ZsCanvasRect,
    cursor: ZsCursor,
    label: Option<String>,
    tooltip: bool,
}

/// How a text button is drawn.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Btn {
    /// Filled in the text color: the one main action of a view.
    Primary,
    /// A quiet filled pill.
    Secondary,
}

impl P {
    pub fn new(pal: Palette) -> Self {
        Self {
            scene: ZsCanvasScene::new(),
            pal,
            regions: Vec::new(),
        }
    }

    /// Fills the part of the canvas that is on screen.
    pub fn fill_visible(&mut self, cx: &ZsCanvasLayoutContext<'_>, color: Color) {
        let v = cx.visible_rect();
        self.fill(v.x.0, v.y.0, v.width.0, v.height.0, color);
    }

    pub fn finish(mut self) -> ZsCanvasScene {
        for region in self.regions {
            let mut declared = ZsCanvasRegion::new(region.id, region.rect).cursor(region.cursor);
            if let Some(label) = region.label {
                declared = declared.label(label);
                if !region.tooltip {
                    declared = declared.without_tooltip();
                }
            }
            self.scene.push_region(declared);
        }
        self.scene
    }

    pub fn fill(&mut self, x: f32, y: f32, w: f32, h: f32, color: Color) {
        if w > 0.0 && h > 0.0 {
            self.scene.push(Prim::fill_rect(
                rect(x, y, w, h),
                NativeDrawFill::Color(color),
            ));
        }
    }

    pub fn round(&mut self, x: f32, y: f32, w: f32, h: f32, radius: f32, color: Color) {
        if w > 0.0 && h > 0.0 {
            self.scene.push(Prim::round_fill(
                rect(x, y, w, h),
                NativeDrawFill::Color(color),
                Dp::new(radius),
            ));
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn round_stroke(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        fill: Color,
        stroke: Color,
    ) {
        self.scene.push(Prim::RoundRect {
            rect: rect(x, y, w, h),
            fill: NativeDrawFill::Color(fill),
            stroke: Some(NativeDrawFill::Color(stroke)),
            radius: Dp::new(radius),
        });
    }

    /// A two-stop gradient from `top` to `bottom`.
    #[allow(clippy::too_many_arguments)]
    pub fn vgradient(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        top: Color,
        bottom: Color,
        radius: f32,
    ) {
        if w > 0.0 && h > 0.0 {
            self.scene.push(Prim::vertical_gradient(
                rect(x, y, w, h),
                NativeDrawFill::Color(top),
                NativeDrawFill::Color(bottom),
                Dp::new(radius),
            ));
        }
    }

    pub fn hline(&mut self, x: f32, y: f32, w: f32, color: Color) {
        self.fill(x, y, w, 1.0, color);
    }

    pub fn vline(&mut self, x: f32, y: f32, h: f32, color: Color) {
        self.fill(x, y, 1.0, h, color);
    }

    pub fn circle(&mut self, cx: f32, cy: f32, radius: f32, color: Color) {
        self.round(
            cx - radius,
            cy - radius,
            radius * 2.0,
            radius * 2.0,
            radius,
            color,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn arc(
        &mut self,
        x: f32,
        y: f32,
        diameter: f32,
        width: f32,
        start: i16,
        sweep: i16,
        color: Color,
    ) {
        self.scene.push(Prim::StrokeArc {
            rect: rect(x, y, diameter, diameter),
            stroke: NativeDrawFill::Color(color),
            width: Dp::new(width),
            start_degrees: start,
            sweep_degrees: sweep,
        });
    }

    pub fn push_clip(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.scene.push(Prim::push_clip(rect(x, y, w, h)));
    }

    pub fn pop_clip(&mut self) {
        self.scene.push(Prim::pop_clip());
    }

    /// Single-line text with ellipsis.
    #[allow(clippy::too_many_arguments)]
    pub fn text_ex(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        size: f32,
        color: Color,
        align: HorizontalAlign,
        weight: TextWeight,
    ) {
        self.text_family(text, x, y, w, h, size, color, align, weight, None);
    }

    /// Single-line text in an explicit font family.
    #[allow(clippy::too_many_arguments)]
    pub fn text_family(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        size: f32,
        color: Color,
        align: HorizontalAlign,
        weight: TextWeight,
        family: Option<&str>,
    ) {
        if text.is_empty() || w <= 2.0 {
            return;
        }
        let mut style = line_style(size, align, weight);
        style.color = self.pal.role_for(color);
        let mut run = Prim::text_run(
            text,
            rect(x, y, w, h),
            style,
            Some(color),
            Some(Dp::new(size)),
        );
        if let Some(family) = family {
            run = run.with_font_family(family);
        }
        self.scene.push(run);
    }

    /// Figures — times, counts, ranks — in the numeral face.
    #[allow(clippy::too_many_arguments)]
    pub fn num(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        size: f32,
        color: Color,
        align: HorizontalAlign,
        weight: TextWeight,
    ) {
        self.text_family(text, x, y, w, h, size, color, align, weight, Some(NUM_FONT));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text(&mut self, text: &str, x: f32, y: f32, w: f32, h: f32, size: f32, color: Color) {
        self.text_ex(
            text,
            x,
            y,
            w,
            h,
            size,
            color,
            HorizontalAlign::Start,
            TextWeight::Regular,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text_b(&mut self, text: &str, x: f32, y: f32, w: f32, h: f32, size: f32, color: Color) {
        self.text_ex(
            text,
            x,
            y,
            w,
            h,
            size,
            color,
            HorizontalAlign::Start,
            TextWeight::Semibold,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text_c(&mut self, text: &str, x: f32, y: f32, w: f32, h: f32, size: f32, color: Color) {
        self.text_ex(
            text,
            x,
            y,
            w,
            h,
            size,
            color,
            HorizontalAlign::Center,
            TextWeight::Regular,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn text_r(&mut self, text: &str, x: f32, y: f32, w: f32, h: f32, size: f32, color: Color) {
        self.text_ex(
            text,
            x,
            y,
            w,
            h,
            size,
            color,
            HorizontalAlign::End,
            TextWeight::Regular,
        );
    }

    /// A page or section heading in the bold face.
    #[allow(clippy::too_many_arguments)]
    pub fn heading(&mut self, text: &str, x: f32, y: f32, w: f32, h: f32, size: f32, color: Color) {
        self.text_ex(
            text,
            x,
            y,
            w,
            h,
            size,
            color,
            HorizontalAlign::Start,
            TextWeight::Bold,
        );
    }

    /// Wrapped paragraph; `style` must match the one used to measure it.
    #[allow(clippy::too_many_arguments)]
    pub fn para(
        &mut self,
        text: &str,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        size: f32,
        style: SemanticTextStyle,
        color: Color,
    ) {
        if text.is_empty() {
            return;
        }
        let mut style = style;
        style.color = self.pal.role_for(color);
        self.scene.push(Prim::text_run(
            text,
            rect(x, y, w, h),
            style,
            Some(color),
            Some(Dp::new(size)),
        ));
    }

    /// One icon-font glyph centered in the box.
    #[allow(clippy::too_many_arguments)]
    pub fn glyph(&mut self, ch: char, x: f32, y: f32, w: f32, h: f32, size: f32, color: Color) {
        if crate::icons::draw(&mut self.scene, ch, x, y, w, h, size, color) {
            return;
        }
        // A box larger than the em square, so glyphs that overhang it (the
        // filled heart, for one) are not clipped by their own text run.
        let side = size * 1.6;
        let mut style = SemanticTextStyle::for_role(TextRole::Body);
        style.color = self.pal.role_for(color);
        style.horizontal_align = HorizontalAlign::Center;
        style.vertical_align = VerticalAlign::Center;
        style.wrap = TextWrap::NoWrap;
        style.ellipsis = false;
        self.scene.push(Prim::text_run(
            ch.to_string(),
            rect(x + (w - side) / 2.0, y + (h - side) / 2.0, side, side),
            style,
            Some(color),
            Some(Dp::new(size)),
        ));
    }

    pub fn image(&mut self, frame: &ZsImageFrame, x: f32, y: f32, w: f32, h: f32) {
        self.scene
            .push(Prim::image(frame.clone(), rect(x, y, w, h)));
    }

    #[allow(clippy::too_many_arguments)]
    pub fn shadow_ex(
        &mut self,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        radius: f32,
        alpha: u8,
        drop: f32,
        blur: f32,
    ) {
        // Many faint layers, each a little tighter: the steps between them
        // stay below what the eye resolves.
        let steps = ((blur / 2.0).round() as usize).clamp(6, 18);
        let base = self.pal.shadow;
        let total = alpha as f32;
        for i in 0..steps {
            let t = (i + 1) as f32 / steps as f32;
            let spread = blur * (1.0 - t);
            // Layers accumulate; together they reach about `total`.
            let a = (total * 2.0 * t / steps as f32).round().clamp(1.0, 255.0) as u8;
            self.round(
                x - spread,
                y - spread + drop * (0.35 + 0.65 * (1.0 - t)),
                w + spread * 2.0,
                h + spread * 2.0,
                radius + spread,
                Color::rgba(base.r, base.g, base.b, a),
            );
        }
    }

    /// A circular icon button: an optional fill, then the glyph.
    #[allow(clippy::too_many_arguments)]
    pub fn icon_button(
        &mut self,
        id: u64,
        glyph: char,
        cx: f32,
        cy: f32,
        d: f32,
        size: f32,
        fill: Option<Color>,
        color: Color,
    ) {
        if let Some(fill) = fill {
            self.circle(cx, cy, d / 2.0, fill);
        }
        self.glyph(glyph, cx - d / 2.0, cy - d / 2.0, d, d, size, color);
        self.hover(id, cx - d / 2.0, cy - d / 2.0, d, d);
    }

    /// A glyph button that fills on hover and reports its tooltip.
    #[allow(clippy::too_many_arguments)]
    pub fn tool(
        &mut self,
        id: u64,
        glyph: char,
        cx: f32,
        cy: f32,
        d: f32,
        size: f32,
        hover: Option<u64>,
        on: bool,
        label: &str,
    ) {
        let pal = self.pal;
        let hot = hover == Some(id);
        let fill = (hot || on).then_some(if on { pal.pressed } else { pal.hover });
        let color = if hot || on { pal.text } else { pal.text2 };
        self.icon_button(id, glyph, cx, cy, d, size, fill, color);
        self.tip(id, label);
    }

    /// A clickable area: hover reports `id` and the pointer becomes a hand.
    pub fn hover(&mut self, id: u64, x: f32, y: f32, w: f32, h: f32) {
        self.region(id, x, y, w, h, ZsCursor::Pointer);
    }

    /// A hover area that keeps the arrow, for rows, window buttons and
    /// backdrops.
    pub fn hover_plain(&mut self, id: u64, x: f32, y: f32, w: f32, h: f32) {
        self.region(id, x, y, w, h, ZsCursor::Arrow);
    }

    fn region(&mut self, id: u64, x: f32, y: f32, w: f32, h: f32, cursor: ZsCursor) {
        self.regions.push(Region {
            id,
            rect: rect(x, y, w, h),
            cursor,
            label: None,
            tooltip: true,
        });
    }

    /// Names the latest region `id` for screen readers and shows `label` as
    /// its tooltip, for buttons drawn as a bare glyph.
    pub fn tip(&mut self, id: u64, label: &str) {
        self.label(id, label, true);
    }

    /// Names the latest region `id` for screen readers only, for areas whose
    /// visible text already says what they do.
    pub fn name(&mut self, id: u64, label: &str) {
        self.label(id, label, false);
    }

    fn label(&mut self, id: u64, label: &str, tooltip: bool) {
        if let Some(region) = self.regions.iter_mut().rev().find(|r| r.id == id) {
            region.label = Some(label.to_string());
            region.tooltip = tooltip;
        }
    }

    /// An indeterminate spinner: a faint ring and a moving quarter arc.
    pub fn spinner(&mut self, x: f32, y: f32, d: f32, phase_ms: u64, color: Color, track: Color) {
        let start = ((phase_ms % 900) * 360 / 900) as i16;
        self.arc(x, y, d, 2.0, 0, 360, track);
        self.arc(x, y, d, 2.0, -start, -100, color);
    }

    /// A rounded "pill" button with a centered label.
    #[allow(clippy::too_many_arguments)]
    pub fn pill(
        &mut self,
        label: &str,
        x: f32,
        y: f32,
        w: f32,
        h: f32,
        fill: Color,
        color: Color,
        size: f32,
    ) {
        self.round(x, y, w, h, h / 2.0, fill);
        self.text_ex(
            label,
            x,
            y,
            w,
            h,
            size,
            color,
            HorizontalAlign::Center,
            TextWeight::Semibold,
        );
    }

    /// A text button at `(x, y)`, optionally led by a glyph; returns its
    /// width. Every button of the player is drawn by this.
    #[allow(clippy::too_many_arguments)]
    pub fn button(
        &mut self,
        cx: &ZsCanvasLayoutContext<'_>,
        id: u64,
        label: &str,
        glyph: Option<char>,
        x: f32,
        y: f32,
        h: f32,
        kind: Btn,
        hover: Option<u64>,
    ) -> f32 {
        let pal = self.pal;
        let size = if h >= 36.0 { 13.5 } else { 12.5 };
        let tw = measure(cx, label, size, TextWeight::Semibold);
        let pad = if h >= 36.0 { 18.0 } else { 14.0 };
        let gw = if glyph.is_some() { size + 8.0 } else { 0.0 };
        let w = pad * 2.0 + gw + tw;
        let hot = hover == Some(id);
        let (fill, color) = match kind {
            Btn::Primary => (
                Some(if hot {
                    mix(pal.control, pal.surface, 0.14)
                } else {
                    pal.control
                }),
                pal.on_control,
            ),
            Btn::Secondary => (Some(if hot { pal.pressed } else { pal.raised }), pal.text),
        };
        if let Some(fill) = fill {
            self.round(x, y, w, h, h / 2.0, fill);
        }
        if let Some(glyph) = glyph {
            self.glyph(glyph, x + pad - 2.0, y, gw, h, size - 1.5, color);
        }
        self.text_ex(
            label,
            x + pad + gw,
            y,
            tw + 4.0,
            h,
            size,
            color,
            HorizontalAlign::Start,
            TextWeight::Semibold,
        );
        self.hover(id, x, y, w, h);
        self.name(id, label);
        w
    }

    /// A round chip: selected chips are filled in the text color.
    #[allow(clippy::too_many_arguments)]
    pub fn chip(
        &mut self,
        cx: &ZsCanvasLayoutContext<'_>,
        id: u64,
        label: &str,
        x: f32,
        y: f32,
        on: bool,
        hover: Option<u64>,
    ) -> f32 {
        let pal = self.pal;
        let h = 30.0;
        let tw = measure(cx, label, 12.5, TextWeight::Regular);
        let w = tw + 26.0;
        let hot = hover == Some(id);
        let (fill, color) = if on {
            (pal.control, pal.on_control)
        } else if hot {
            (pal.pressed, pal.text)
        } else {
            (pal.raised, pal.text2)
        };
        self.round(x, y, w, h, h / 2.0, fill);
        self.text_ex(
            label,
            x,
            y,
            w,
            h,
            12.5,
            color,
            HorizontalAlign::Center,
            if on {
                TextWeight::Semibold
            } else {
                TextWeight::Regular
            },
        );
        self.hover(id, x, y, w, h);
        w
    }

    /// A segmented switch: labels in a recessed track, the selected one on a
    /// raised thumb. Region ids are `rid(kind, index)`. Returns the width.
    #[allow(clippy::too_many_arguments)]
    pub fn segmented(
        &mut self,
        cx: &ZsCanvasLayoutContext<'_>,
        kind: u32,
        labels: &[&str],
        selected: usize,
        x: f32,
        y: f32,
        hover: Option<u64>,
    ) -> f32 {
        let pal = self.pal;
        let h = 32.0;
        let widths: Vec<f32> = labels
            .iter()
            .map(|l| measure(cx, l, 12.5, TextWeight::Semibold) + 24.0)
            .collect();
        let total: f32 = widths.iter().sum::<f32>() + 6.0;
        self.round(x, y, total, h, h / 2.0, pal.raised);
        let mut sx = x + 3.0;
        for (i, (label, w)) in labels.iter().zip(&widths).enumerate() {
            let region = rid(kind, i as u32);
            let on = i == selected;
            let hot = hover == Some(region);
            if on {
                self.round(
                    sx,
                    y + 3.0,
                    *w,
                    h - 6.0,
                    (h - 6.0) / 2.0,
                    if pal.dark {
                        pal.line_strong
                    } else {
                        pal.surface
                    },
                );
                if !pal.dark {
                    self.round_stroke(
                        sx,
                        y + 3.0,
                        *w,
                        h - 6.0,
                        (h - 6.0) / 2.0,
                        pal.surface,
                        with_alpha(pal.line_strong, 0xff),
                    );
                }
            }
            self.text_ex(
                label,
                sx,
                y,
                *w,
                h,
                12.5,
                if on || hot { pal.text } else { pal.text2 },
                HorizontalAlign::Center,
                TextWeight::Semibold,
            );
            self.hover(region, sx, y + 2.0, *w, h - 4.0);
            sx += w;
        }
        total
    }

    /// A section heading with an optional trailing link ("全部 ›"); the link
    /// reports `link`.
    #[allow(clippy::too_many_arguments)]
    pub fn section(
        &mut self,
        cx: &ZsCanvasLayoutContext<'_>,
        title: &str,
        x: f32,
        y: f32,
        w: f32,
        link: Option<(u64, &str)>,
        hover: Option<u64>,
    ) {
        let pal = self.pal;
        self.heading(title, x, y, w - 120.0, 28.0, 19.0, pal.text);
        if let Some((id, label)) = link {
            let tw = measure(cx, label, 12.5, TextWeight::Regular);
            let lx = x + w - tw - 18.0;
            let hot = hover == Some(id);
            let color = if hot { pal.text } else { pal.text3 };
            self.text(label, lx, y, tw + 2.0, 28.0, 12.5, color);
            self.glyph(
                crate::theme::glyph::CHEVRON_RIGHT,
                lx + tw + 2.0,
                y,
                14.0,
                28.0,
                9.0,
                color,
            );
            self.hover(id, lx - 6.0, y, tw + 26.0, 28.0);
        }
    }
}

/// Measures a single line of text at `size` (DP width), matching `P::text_ex`.
pub fn measure(cx: &ZsCanvasLayoutContext<'_>, text: &str, size: f32, weight: TextWeight) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let style = line_style(size, HorizontalAlign::Start, weight);
    cx.measure_text_at(text, style, Some(Dp::new(size)), None)
        .width
        .0
}

/// Measures figures drawn with [`P::num`].
pub fn measure_num(
    cx: &ZsCanvasLayoutContext<'_>,
    text: &str,
    size: f32,
    weight: TextWeight,
) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let style = line_style(size, HorizontalAlign::Start, weight);
    cx.measure_text_with(text, style, Some(Dp::new(size)), Some(NUM_FONT), None)
        .width
        .0
}
