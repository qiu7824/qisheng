//! Declarative touch regions for the Android retained host.
use crate::{Dp, WidgetId, ZsuiError, ZsuiResult};
use serde::Serialize;

/// Surface-local logical rectangle. All coordinates are device independent.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AndroidTouchRect {
    pub x: Dp,
    pub y: Dp,
    pub width: Dp,
    pub height: Dp,
}

impl AndroidTouchRect {
    pub const fn new(x: Dp, y: Dp, width: Dp, height: Dp) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }
    fn valid(self) -> bool {
        [self.x.0, self.y.0, self.width.0, self.height.0]
            .iter()
            .all(|v| v.is_finite() && v.abs() <= 32768.0)
            && self.width.0 >= 0.0
            && self.height.0 >= 0.0
    }
    pub(crate) fn contains(self, x: f32, y: f32) -> bool {
        x >= self.x.0
            && x < self.x.0 + self.width.0
            && y >= self.y.0
            && y < self.y.0 + self.height.0
    }
}

/// A controlled vertical viewport. The application owns its offset; the host
/// supplies bounded drag/fling deltas through the ordinary scroll input route.
#[derive(Debug, Clone, PartialEq)]
pub struct AndroidScrollArea {
    pub id: u64,
    pub rect: AndroidTouchRect,
    pub offset: Dp,
    pub max_offset: Dp,
}

/// An opt-in horizontal swipe area targeting a Canvas's typed swipe callback.
/// Exclusions retain normal pointer capture, for example a seek or range slider.
#[derive(Debug, Clone, PartialEq)]
pub struct AndroidSwipeArea {
    pub widget: WidgetId,
    pub rect: AndroidTouchRect,
    pub exclusions: Vec<AndroidTouchRect>,
    pub minimum_distance: Dp,
}

impl AndroidSwipeArea {
    pub fn new(widget: WidgetId, rect: AndroidTouchRect) -> Self {
        Self {
            widget,
            rect,
            exclusions: Vec::new(),
            minimum_distance: Dp::new(64.0),
        }
    }
}

/// A bounded interaction declaration. Change `context` when replacing a page
/// or the meaning of its regions; this cancels stale capture and kinetic motion.
/// Later scroll areas win when rectangles overlap.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AndroidInteractions {
    pub context: u64,
    pub scrolls: Vec<AndroidScrollArea>,
    pub swipes: Vec<AndroidSwipeArea>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AndroidInteractionFrame {
    pub context: String,
    pub scrolls: Vec<AndroidScrollFrame>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AndroidScrollFrame {
    pub id: String,
    pub rect: [f32; 4],
    pub offset: f32,
    pub max_offset: f32,
}

impl AndroidInteractions {
    pub(crate) fn validate(&self) -> ZsuiResult<()> {
        let invalid = self.scrolls.len() > 32
            || self.swipes.len() > 32
            || self.scrolls.iter().any(|area| {
                !area.rect.valid()
                    || !area.offset.0.is_finite()
                    || !area.max_offset.0.is_finite()
                    || area.offset.0 < 0.0
                    || area.max_offset.0 < 0.0
                    || area.offset.0 > area.max_offset.0
                    || area.max_offset.0 > 10_000_000.0
            })
            || self
                .scrolls
                .iter()
                .enumerate()
                .any(|(i, a)| self.scrolls[..i].iter().any(|b| a.id == b.id))
            || self.swipes.iter().any(|area| {
                !area.rect.valid()
                    || area.exclusions.len() > 32
                    || area.exclusions.iter().any(|rect| !rect.valid())
                    || !area.minimum_distance.0.is_finite()
                    || !(8.0..=4096.0).contains(&area.minimum_distance.0)
            });
        #[cfg(not(feature = "canvas"))]
        let invalid = invalid || !self.swipes.is_empty();
        if invalid {
            return Err(ZsuiError::invalid_spec(
                "android_interactions",
                "invalid touch regions or range",
            ));
        }
        Ok(())
    }

    pub(crate) fn frame(&self, density: f32) -> AndroidInteractionFrame {
        AndroidInteractionFrame {
            context: self.context.to_string(),
            scrolls: self
                .scrolls
                .iter()
                .map(|area| AndroidScrollFrame {
                    id: area.id.to_string(),
                    rect: [
                        area.rect.x.0 * density,
                        area.rect.y.0 * density,
                        area.rect.width.0 * density,
                        area.rect.height.0 * density,
                    ],
                    offset: area.offset.0 * density,
                    max_offset: area.max_offset.0 * density,
                })
                .collect(),
        }
    }

    pub(crate) fn swipe_at(&self, x: f32, y: f32) -> Option<AndroidSwipeArea> {
        self.swipes
            .iter()
            .rev()
            .find(|area| {
                area.rect.contains(x, y) && !area.exclusions.iter().any(|r| r.contains(x, y))
            })
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn area() -> AndroidScrollArea {
        AndroidScrollArea {
            id: 7,
            rect: AndroidTouchRect::new(
                Dp::new(10.0),
                Dp::new(20.0),
                Dp::new(200.0),
                Dp::new(300.0),
            ),
            offset: Dp::new(40.0),
            max_offset: Dp::new(9000.0),
        }
    }
    #[test]
    fn physical_scroll_bounds_follow_density_and_keep_controlled_offset() {
        let config = AndroidInteractions {
            context: 42,
            scrolls: vec![area()],
            swipes: vec![],
        };
        config.validate().unwrap();
        let frame = config.frame(2.5);
        assert_eq!(frame.context, "42");
        assert_eq!(frame.scrolls[0].rect, [25.0, 50.0, 500.0, 750.0]);
        assert_eq!(frame.scrolls[0].offset, 100.0);
        assert_eq!(frame.scrolls[0].max_offset, 22500.0);
    }
    #[test]
    fn malformed_and_duplicate_scroll_ranges_are_rejected() {
        for bad in [f32::NAN, f32::INFINITY, -1.0] {
            let mut a = area();
            a.max_offset = Dp::new(bad);
            assert!(AndroidInteractions {
                scrolls: vec![a],
                ..Default::default()
            }
            .validate()
            .is_err());
        }
        assert!(AndroidInteractions {
            scrolls: vec![area(), area()],
            ..Default::default()
        }
        .validate()
        .is_err());
        let mut a = area();
        a.offset = Dp::new(10000.0);
        assert!(AndroidInteractions {
            scrolls: vec![a],
            ..Default::default()
        }
        .validate()
        .is_err());
    }
}
