//! Vector icons drawn with the egui painter on a 24×24 design grid.
//!
//! Font glyphs vary by platform (and egui's bundled fonts lack most of them), so the
//! chrome draws its own line icons: 1.7-unit strokes, round joins, scaled to any size.

use egui::epaint::{PathShape, PathStroke};
use egui::{Color32, Painter, Pos2, Rect, Shape, Stroke, vec2};

/// Every icon the UI uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Icon {
    Logo,
    Home,
    Library,
    Search,
    Settings,
    ChevronLeft,
    ChevronRight,
    ChevronDown,
    SidebarCollapse,
    SidebarExpand,
    Grid,
    Filter,
    Sort,
    Close,
    Play,
    Refresh,
    Gamepad,
    Person,
    SignOut,
    Check,
    Clock,
    Sparkle,
    Fullscreen,
    ExitFullscreen,
    Gauge,
    Globe,
    Monitor,
    Info,
    Users,
    Touch,
    Cloud,
    Copy,
    External,
    Heart,
    HeartFilled,
}

/// Paints `icon` centred in `rect` (the shorter side is the icon size).
pub fn paint(painter: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let size = rect.width().min(rect.height());
    let origin = rect.center() - vec2(size, size) / 2.0;
    let s = size / 24.0;
    let p = |x: f32, y: f32| -> Pos2 { origin + vec2(x * s, y * s) };
    let w = (1.7 * s).max(1.1);
    let stroke = Stroke::new(w, color);
    let line = |a: Pos2, b: Pos2| {
        painter.line_segment([a, b], stroke);
    };
    let poly = |pts: Vec<Pos2>, closed: bool| {
        let shape = if closed {
            PathShape::closed_line(pts, stroke)
        } else {
            PathShape::line(pts, stroke)
        };
        painter.add(shape);
    };
    let rrect = |r: Rect, radius: f32| {
        painter.rect_stroke(
            r,
            egui::CornerRadius::same((radius * s) as u8),
            stroke,
            egui::StrokeKind::Middle,
        );
    };
    let r = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(p(x0, y0), p(x1, y1));

    match icon {
        Icon::Logo => {
            // A rounded tile with a cloud-play mark: the OpenXCloud badge.
            let badge = r(1.0, 1.0, 23.0, 23.0);
            painter.rect_filled(
                badge,
                egui::CornerRadius::same((7.0 * s) as u8),
                crate::theme::ACCENT,
            );
            let ink = crate::theme::ON_ACCENT;
            painter.add(PathShape::convex_polygon(
                vec![p(9.5, 7.0), p(17.5, 12.0), p(9.5, 17.0)],
                ink,
                Stroke::NONE,
            ));
        }
        Icon::Home => {
            poly(
                vec![
                    p(4.0, 10.5),
                    p(12.0, 4.0),
                    p(20.0, 10.5),
                    p(20.0, 20.0),
                    p(14.5, 20.0),
                    p(14.5, 14.5),
                    p(9.5, 14.5),
                    p(9.5, 20.0),
                    p(4.0, 20.0),
                ],
                true,
            );
        }
        Icon::Library => {
            rrect(r(4.0, 5.0, 7.5, 20.0), 1.0);
            rrect(r(9.5, 5.0, 13.0, 20.0), 1.0);
            poly(
                vec![p(15.2, 6.2), p(18.6, 5.4), p(21.0, 19.2), p(17.6, 20.0)],
                true,
            );
        }
        Icon::Search => {
            painter.circle_stroke(p(10.5, 10.5), 6.0 * s, stroke);
            line(p(15.0, 15.0), p(20.0, 20.0));
        }
        Icon::Settings => {
            let c = p(12.0, 12.0);
            painter.circle_stroke(c, 3.0 * s, stroke);
            let teeth = 8;
            let mut pts = Vec::with_capacity(teeth * 4);
            for i in 0..teeth {
                let a0 = i as f32 / teeth as f32 * std::f32::consts::TAU;
                let step = std::f32::consts::TAU / teeth as f32;
                for (frac, radius) in [(0.0, 8.6), (0.18, 8.6), (0.32, 6.6), (0.86, 6.6)] {
                    let a = a0 + step * frac - step * 0.09;
                    pts.push(c + vec2(a.cos(), a.sin()) * radius * s);
                }
            }
            poly(pts, true);
        }
        Icon::ChevronLeft => poly(vec![p(15.0, 5.0), p(8.0, 12.0), p(15.0, 19.0)], false),
        Icon::ChevronRight => poly(vec![p(9.0, 5.0), p(16.0, 12.0), p(9.0, 19.0)], false),
        Icon::ChevronDown => poly(vec![p(6.0, 9.0), p(12.0, 15.0), p(18.0, 9.0)], false),
        Icon::SidebarCollapse | Icon::SidebarExpand => {
            rrect(r(3.5, 5.0, 20.5, 19.0), 2.5);
            line(p(9.5, 5.0), p(9.5, 19.0));
            if icon == Icon::SidebarCollapse {
                poly(vec![p(15.5, 9.5), p(13.0, 12.0), p(15.5, 14.5)], false);
            } else {
                poly(vec![p(13.0, 9.5), p(15.5, 12.0), p(13.0, 14.5)], false);
            }
        }
        Icon::Grid => {
            rrect(r(4.0, 4.0, 10.5, 10.5), 1.5);
            rrect(r(13.5, 4.0, 20.0, 10.5), 1.5);
            rrect(r(4.0, 13.5, 10.5, 20.0), 1.5);
            rrect(r(13.5, 13.5, 20.0, 20.0), 1.5);
        }
        Icon::Filter => {
            line(p(4.0, 7.0), p(20.0, 7.0));
            line(p(7.0, 12.0), p(17.0, 12.0));
            line(p(10.0, 17.0), p(14.0, 17.0));
        }
        Icon::Sort => {
            line(p(7.0, 4.5), p(7.0, 19.5));
            poly(vec![p(4.0, 16.5), p(7.0, 19.5), p(10.0, 16.5)], false);
            line(p(12.5, 6.0), p(20.0, 6.0));
            line(p(12.5, 11.0), p(18.0, 11.0));
            line(p(12.5, 16.0), p(16.0, 16.0));
        }
        Icon::Close => {
            line(p(6.0, 6.0), p(18.0, 18.0));
            line(p(18.0, 6.0), p(6.0, 18.0));
        }
        Icon::Play => {
            let pts = vec![p(7.5, 5.0), p(19.0, 12.0), p(7.5, 19.0)];
            painter.add(PathShape {
                points: pts,
                closed: true,
                fill: color,
                stroke: PathStroke::new(w * 0.8, color),
            });
        }
        Icon::Refresh => {
            arc(painter, p(12.0, 12.0), 7.0 * s, -0.6, 4.4, stroke);
            let end_a: f32 = -0.6;
            let tip = p(12.0, 12.0) + vec2(end_a.cos(), end_a.sin()) * 7.0 * s;
            poly(
                vec![tip + vec2(-4.0, -1.0) * s, tip, tip + vec2(0.5, -4.2) * s],
                false,
            );
        }
        Icon::Gamepad => {
            let body = vec![
                p(7.0, 7.0),
                p(17.0, 7.0),
                p(21.0, 16.0),
                p(19.5, 18.5),
                p(16.5, 17.0),
                p(14.5, 14.5),
                p(9.5, 14.5),
                p(7.5, 17.0),
                p(4.5, 18.5),
                p(3.0, 16.0),
            ];
            poly(body, true);
            line(p(7.0, 10.0), p(7.0, 12.5));
            line(p(5.75, 11.25), p(8.25, 11.25));
            painter.circle_filled(p(16.0, 10.2), 1.0 * s, color);
            painter.circle_filled(p(17.8, 12.0), 1.0 * s, color);
        }
        Icon::Person => {
            painter.circle_stroke(p(12.0, 8.5), 3.8 * s, stroke);
            arc(painter, p(12.0, 21.5), 7.5 * s, 3.55, 5.87, stroke);
        }
        Icon::SignOut => {
            poly(
                vec![p(10.0, 5.0), p(5.0, 5.0), p(5.0, 19.0), p(10.0, 19.0)],
                false,
            );
            line(p(10.0, 12.0), p(20.0, 12.0));
            poly(vec![p(16.5, 8.5), p(20.0, 12.0), p(16.5, 15.5)], false);
        }
        Icon::Check => poly(vec![p(5.0, 12.5), p(10.0, 17.5), p(19.0, 7.0)], false),
        Icon::Clock => {
            painter.circle_stroke(p(12.0, 12.0), 8.0 * s, stroke);
            poly(vec![p(12.0, 7.5), p(12.0, 12.0), p(15.5, 14.0)], false);
        }
        Icon::Sparkle => {
            let c = p(12.0, 12.0);
            let star = |cx: Pos2, big: f32, small: f32| {
                vec![
                    cx + vec2(0.0, -big) * s,
                    cx + vec2(small, -small) * s,
                    cx + vec2(big, 0.0) * s,
                    cx + vec2(small, small) * s,
                    cx + vec2(0.0, big) * s,
                    cx + vec2(-small, small) * s,
                    cx + vec2(-big, 0.0) * s,
                    cx + vec2(-small, -small) * s,
                ]
            };
            poly(star(c + vec2(-1.5, 1.0) * s, 7.0, 1.8), true);
            poly(star(p(18.0, 5.5), 3.0, 0.8), true);
        }
        Icon::Fullscreen => {
            poly(vec![p(4.0, 9.0), p(4.0, 4.0), p(9.0, 4.0)], false);
            poly(vec![p(15.0, 4.0), p(20.0, 4.0), p(20.0, 9.0)], false);
            poly(vec![p(20.0, 15.0), p(20.0, 20.0), p(15.0, 20.0)], false);
            poly(vec![p(9.0, 20.0), p(4.0, 20.0), p(4.0, 15.0)], false);
        }
        Icon::ExitFullscreen => {
            poly(vec![p(4.0, 9.0), p(9.0, 9.0), p(9.0, 4.0)], false);
            poly(vec![p(15.0, 4.0), p(15.0, 9.0), p(20.0, 9.0)], false);
            poly(vec![p(20.0, 15.0), p(15.0, 15.0), p(15.0, 20.0)], false);
            poly(vec![p(9.0, 20.0), p(9.0, 15.0), p(4.0, 15.0)], false);
        }
        Icon::Gauge => {
            arc(painter, p(12.0, 15.0), 8.5 * s, 3.4, 6.03, stroke);
            line(p(12.0, 15.0), p(16.0, 9.5));
            painter.circle_filled(p(12.0, 15.0), 1.4 * s, color);
        }
        Icon::Globe => {
            let c = p(12.0, 12.0);
            painter.circle_stroke(c, 8.0 * s, stroke);
            line(p(4.0, 12.0), p(20.0, 12.0));
            painter.add(Shape::ellipse_stroke(c, vec2(3.6, 8.0) * s, stroke));
        }
        Icon::Monitor => {
            rrect(r(3.5, 4.5, 20.5, 16.0), 2.0);
            line(p(12.0, 16.0), p(12.0, 19.5));
            line(p(8.0, 19.5), p(16.0, 19.5));
        }
        Icon::Info => {
            painter.circle_stroke(p(12.0, 12.0), 8.0 * s, stroke);
            line(p(12.0, 11.0), p(12.0, 16.5));
            painter.circle_filled(p(12.0, 7.8), 1.1 * s, color);
        }
        Icon::Users => {
            painter.circle_stroke(p(9.0, 9.0), 3.2 * s, stroke);
            arc(painter, p(9.0, 20.5), 6.2 * s, 3.55, 5.87, stroke);
            arc(painter, p(16.0, 9.0), 3.2 * s, -1.6, 1.6, stroke);
            arc(painter, p(16.5, 20.5), 5.2 * s, 4.7, 5.87, stroke);
        }
        Icon::Touch => {
            rrect(r(7.0, 3.5, 17.0, 20.5), 2.5);
            line(p(10.5, 17.5), p(13.5, 17.5));
        }
        Icon::Cloud => {
            let pts = vec![
                p(7.0, 18.0),
                p(4.0, 15.5),
                p(4.8, 11.8),
                p(8.0, 10.5),
                p(10.5, 6.8),
                p(15.0, 6.8),
                p(17.4, 10.0),
                p(20.0, 12.0),
                p(19.6, 16.4),
                p(16.5, 18.0),
            ];
            poly(pts, true);
        }
        Icon::Copy => {
            rrect(r(8.5, 8.5, 19.5, 19.5), 2.0);
            poly(
                vec![
                    p(15.5, 5.5),
                    p(15.5, 4.5),
                    p(4.5, 4.5),
                    p(4.5, 15.5),
                    p(5.5, 15.5),
                ],
                false,
            );
        }
        Icon::Heart | Icon::HeartFilled => {
            // The classic parametric heart, scaled into the 24-unit box.
            let steps = 48;
            let pts: Vec<Pos2> = (0..steps)
                .map(|i| {
                    let t = i as f32 / steps as f32 * std::f32::consts::TAU;
                    let x = 16.0 * t.sin().powi(3);
                    let y = 13.0 * t.cos()
                        - 5.0 * (2.0 * t).cos()
                        - 2.0 * (3.0 * t).cos()
                        - (4.0 * t).cos();
                    p(12.0 + x * 0.55, 11.6 - y * 0.55)
                })
                .collect();
            if icon == Icon::HeartFilled {
                // Star-shaped around its middle: a triangle fan fills it.
                let mut mesh = egui::Mesh::default();
                mesh.colored_vertex(p(12.0, 11.5), color);
                for q in &pts {
                    mesh.colored_vertex(*q, color);
                }
                let n = pts.len() as u32;
                for i in 0..n {
                    mesh.add_triangle(0, 1 + i, 1 + (i + 1) % n);
                }
                painter.add(Shape::mesh(mesh));
            }
            poly(pts, true);
        }
        Icon::External => {
            poly(
                vec![
                    p(18.0, 13.0),
                    p(18.0, 19.0),
                    p(5.0, 19.0),
                    p(5.0, 6.0),
                    p(11.0, 6.0),
                ],
                false,
            );
            line(p(11.0, 13.0), p(19.5, 4.5));
            poly(vec![p(14.0, 4.5), p(19.5, 4.5), p(19.5, 10.0)], false);
        }
    }
}

/// A circular arc from `a0` to `a1` (radians, y down) as a polyline.
fn arc(painter: &Painter, center: Pos2, radius: f32, a0: f32, a1: f32, stroke: Stroke) {
    let steps = 24;
    let pts: Vec<Pos2> = (0..=steps)
        .map(|i| {
            let a = a0 + (a1 - a0) * i as f32 / steps as f32;
            center + vec2(a.cos(), a.sin()) * radius
        })
        .collect();
    painter.add(PathShape::line(pts, stroke));
}

/// A `size`×`size` rect at `pos` (top-left).
pub fn square(pos: Pos2, size: f32) -> Rect {
    Rect::from_min_size(pos, vec2(size, size))
}

/// Centre point helper for callers that lay out icons next to text.
pub fn centered(center: Pos2, size: f32) -> Rect {
    Rect::from_center_size(center, vec2(size, size))
}
