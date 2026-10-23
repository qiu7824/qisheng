//! Player icons drawn in logical coordinates without a platform icon font.
use crate::theme::glyph as g;
use zsui::{Color, Dp, NativeDrawFill, ZsCanvasPoint, ZsCanvasPrimitive, ZsCanvasScene};

struct Pen<'a> {
    scene: &'a mut ZsCanvasScene,
    x: f32,
    y: f32,
    scale: f32,
    color: Color,
}
impl Pen<'_> {
    fn points(&self, points: &[(f32, f32)]) -> Vec<ZsCanvasPoint> {
        points
            .iter()
            .map(|&(x, y)| {
                ZsCanvasPoint::new(
                    Dp::new(self.x + x * self.scale),
                    Dp::new(self.y + y * self.scale),
                )
            })
            .collect()
    }
    fn line(&mut self, points: &[(f32, f32)]) {
        let points = self.points(points);
        self.scene.push(ZsCanvasPrimitive::polyline(
            points,
            NativeDrawFill::Color(self.color),
            Dp::new(1.7 * self.scale),
        ));
    }
    fn polygon(&mut self, points: &[(f32, f32)], fill: bool) {
        let points = self.points(points);
        self.scene.push(if fill {
            ZsCanvasPrimitive::polygon(points, NativeDrawFill::Color(self.color))
        } else {
            ZsCanvasPrimitive::polygon_outline(
                points,
                NativeDrawFill::Color(self.color),
                Dp::new(1.7 * self.scale),
            )
        });
    }
    fn box_at(&mut self, x: f32, y: f32, w: f32, h: f32, fill: bool) {
        self.polygon(&[(x, y), (x + w, y), (x + w, y + h), (x, y + h)], fill);
    }
    fn ellipse(&mut self, x: f32, y: f32, rx: f32, ry: f32, fill: bool) {
        let points = (0..32)
            .map(|i| {
                let a = i as f32 * std::f32::consts::TAU / 32.0;
                (x + rx * a.cos(), y + ry * a.sin())
            })
            .collect::<Vec<_>>();
        self.polygon(&points, fill);
    }
    fn arc(&mut self, x: f32, y: f32, r: f32, start: f32, end: f32) {
        let points = (0..17)
            .map(|i| {
                let a = (start + (end - start) * i as f32 / 16.0).to_radians();
                (x + r * a.cos(), y + r * a.sin())
            })
            .collect::<Vec<_>>();
        self.line(&points);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw(
    scene: &mut ZsCanvasScene,
    glyph: char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    size: f32,
    color: Color,
) -> bool {
    let side = size.max(1.0);
    let mut p = Pen {
        scene,
        x: x + (w - side) / 2.0,
        y: y + (h - side) / 2.0,
        scale: side / 24.0,
        color,
    };
    match glyph {
        g::PLAY => p.polygon(&[(7., 3.), (21., 12.), (7., 21.)], true),
        g::PAUSE => {
            p.box_at(6., 3., 4., 18., true);
            p.box_at(14., 3., 4., 18., true);
        }
        g::PREVIOUS => {
            p.box_at(3., 4., 2.5, 16., true);
            p.polygon(&[(20., 4.), (7., 12.), (20., 20.)], true);
        }
        g::NEXT => {
            p.box_at(18.5, 4., 2.5, 16., true);
            p.polygon(&[(4., 4.), (17., 12.), (4., 20.)], true);
        }
        g::SHUFFLE => {
            p.line(&[(3., 6.), (7., 6.), (17., 18.), (21., 18.)]);
            p.line(&[(3., 18.), (7., 18.), (17., 6.), (21., 6.)]);
            p.line(&[(18., 3.), (21., 6.), (18., 9.)]);
            p.line(&[(18., 15.), (21., 18.), (18., 21.)]);
        }
        g::REPEAT_ALL | g::REPEAT_ONE => {
            p.line(&[(4., 10.), (4., 6.), (20., 6.)]);
            p.line(&[(17., 3.), (20., 6.), (17., 9.)]);
            p.line(&[(20., 14.), (20., 18.), (4., 18.)]);
            p.line(&[(7., 15.), (4., 18.), (7., 21.)]);
            if glyph == g::REPEAT_ONE {
                p.line(&[(10., 11.), (12., 9.), (12., 15.)]);
            }
        }
        g::ORDER | g::QUEUE => {
            p.line(&[(3., 5.), (20., 5.)]);
            p.line(&[(3., 10.), (20., 10.)]);
            p.line(&[(3., 15.), (12., 15.)]);
            if glyph == g::QUEUE {
                p.polygon(&[(17., 14.), (22., 18.), (17., 22.)], true);
            } else {
                p.line(&[(3., 20.), (12., 20.)]);
            }
        }
        g::STOP_AFTER => {
            p.ellipse(12., 12., 9., 9., false);
            p.box_at(8., 8., 8., 8., true);
        }
        g::VOLUME | g::VOLUME0 | g::VOLUME1 | g::MUTE => {
            p.polygon(
                &[
                    (3., 9.),
                    (7., 9.),
                    (12., 5.),
                    (12., 19.),
                    (7., 15.),
                    (3., 15.),
                ],
                false,
            );
            if glyph == g::MUTE {
                p.line(&[(16., 9.), (22., 15.)]);
                p.line(&[(16., 15.), (22., 9.)]);
            } else if glyph != g::VOLUME0 {
                p.arc(11., 12., 6., -50., 50.);
                if glyph == g::VOLUME {
                    p.arc(11., 12., 10., -50., 50.);
                }
            }
        }
        g::HEART | g::HEART_FILL => {
            let points = (0..64)
                .map(|i| {
                    let t = i as f32 * std::f32::consts::TAU / 64.0;
                    (
                        12. + 0.6 * 16. * t.sin().powi(3),
                        11. - 0.6
                            * (13. * t.cos()
                                - 5. * (2. * t).cos()
                                - 2. * (3. * t).cos()
                                - (4. * t).cos()),
                    )
                })
                .collect::<Vec<_>>();
            p.polygon(&points, glyph == g::HEART_FILL);
        }
        g::NOTE => {
            p.line(&[(15., 18.), (15., 3.), (21., 5.)]);
            p.ellipse(11.5, 18., 3.5, 2.5, true);
        }
        g::NOTES => {
            p.line(&[(8., 18.), (8., 6.), (19., 3.), (19., 16.)]);
            p.line(&[(8., 9.), (19., 6.)]);
            p.ellipse(5., 18., 3., 2.3, true);
            p.ellipse(16., 16., 3., 2.3, true);
        }
        g::SEARCH => {
            p.ellipse(10., 10., 6.5, 6.5, false);
            p.line(&[(15., 15.), (21., 21.)]);
        }
        g::FOLDER => p.polygon(
            &[
                (3., 5.),
                (9., 5.),
                (12., 8.),
                (21., 8.),
                (21., 20.),
                (3., 20.),
            ],
            false,
        ),
        g::SETTINGS => {
            let points = (0..32)
                .map(|i| {
                    let angle = i as f32 * std::f32::consts::TAU / 32.0;
                    let r = if i % 4 < 2 { 10. } else { 8. };
                    (12. + r * angle.cos(), 12. + r * angle.sin())
                })
                .collect::<Vec<_>>();
            p.polygon(&points, false);
            p.ellipse(12., 12., 3.3, 3.3, false);
        }
        g::SOURCE => {
            p.line(&[(8., 5.), (2., 12.), (8., 19.)]);
            p.line(&[(16., 5.), (22., 12.), (16., 19.)]);
            p.line(&[(14., 3.), (10., 21.)]);
        }
        g::ADD => {
            p.line(&[(4., 12.), (20., 12.)]);
            p.line(&[(12., 4.), (12., 20.)]);
        }
        g::MORE => {
            for y in [5., 12., 19.] {
                p.ellipse(12., y, 1.7, 1.7, true);
            }
        }
        g::CANCEL | g::CLOSE => {
            p.line(&[(5., 5.), (19., 19.)]);
            p.line(&[(5., 19.), (19., 5.)]);
        }
        g::CHECK => p.line(&[(3., 12.), (9., 18.), (21., 5.)]),
        g::INFO => {
            p.ellipse(12., 12., 9., 9., false);
            p.ellipse(12., 7., 1., 1., true);
            p.line(&[(12., 11.), (12., 17.)]);
        }
        g::WARNING => {
            p.polygon(&[(12., 3.), (22., 21.), (2., 21.)], false);
            p.line(&[(12., 9.), (12., 14.)]);
            p.ellipse(12., 18., 1., 1., true);
        }
        g::SHIELD => {
            p.polygon(
                &[
                    (12., 2.),
                    (21., 6.),
                    (19., 16.),
                    (12., 22.),
                    (5., 16.),
                    (3., 6.),
                ],
                false,
            );
            p.line(&[(7., 11.), (11., 15.), (17., 8.)]);
        }
        g::MIN => p.line(&[(4., 12.), (20., 12.)]),
        g::MAX => p.box_at(4., 4., 16., 16., false),
        g::RESTORE => {
            p.box_at(3., 7., 14., 14., false);
            p.line(&[(7., 7.), (7., 3.), (21., 3.), (21., 17.), (17., 17.)]);
        }
        g::TIMER => {
            p.ellipse(12., 13., 8., 8., false);
            p.line(&[(9., 2.), (15., 2.)]);
            p.line(&[(12., 2.), (12., 5.)]);
            p.line(&[(12., 8.), (12., 13.), (16., 15.)]);
        }
        g::CHEVRON_DOWN => p.line(&[(5., 8.), (12., 15.), (19., 8.)]),
        g::CHEVRON_LEFT => p.line(&[(15., 5.), (8., 12.), (15., 19.)]),
        g::CHEVRON_RIGHT => p.line(&[(9., 5.), (16., 12.), (9., 19.)]),
        g::HOME | g::HOME_FILL => p.polygon(
            &[
                (3., 11.),
                (12., 3.),
                (21., 11.),
                (21., 21.),
                (15., 21.),
                (15., 14.),
                (9., 14.),
                (9., 21.),
                (3., 21.),
            ],
            glyph == g::HOME_FILL,
        ),
        g::GRID => {
            for (x, y) in [(3., 3.), (14., 3.), (3., 14.), (14., 14.)] {
                p.box_at(x, y, 7., 7., false);
            }
        }
        g::CHART => {
            p.box_at(3., 14., 4., 7., true);
            p.box_at(10., 9., 4., 12., true);
            p.box_at(17., 3., 4., 18., true);
        }
        g::ALBUM => {
            p.box_at(2., 2., 20., 20., false);
            p.ellipse(12., 12., 6., 6., false);
            p.ellipse(12., 12., 1., 1., true);
        }
        g::ARTIST => {
            p.ellipse(12., 7., 4., 4., false);
            p.line(&[
                (3., 22.),
                (4., 18.),
                (8., 14.),
                (16., 14.),
                (20., 18.),
                (21., 22.),
            ]);
        }
        g::DISC => {
            p.ellipse(12., 12., 10., 10., false);
            p.ellipse(12., 12., 2.5, 2.5, false);
            p.arc(12., 12., 7., 200., 250.);
        }
        g::GLOBE => {
            p.ellipse(12., 12., 10., 10., false);
            p.ellipse(12., 12., 4.5, 10., false);
            p.line(&[(2., 12.), (22., 12.)]);
        }
        g::EXPAND => {
            for points in [
                [(3., 9.), (3., 3.), (9., 3.)],
                [(15., 3.), (21., 3.), (21., 9.)],
                [(3., 15.), (3., 21.), (9., 21.)],
                [(15., 21.), (21., 21.), (21., 15.)],
            ] {
                p.line(&points);
            }
        }
        _ => return false,
    }
    true
}
