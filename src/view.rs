//! Scrolling and zooming of the timelines (piano roll, channel rack): the
//! mouse wheel read from the raw events (egui turns Ctrl+wheel into its own
//! zoom), and zoom around the pointer.
//!
//! - wheel: up and down; Shift+wheel (or a horizontal wheel): along the time;
//! - Ctrl+wheel: horizontal zoom, Alt+wheel: vertical zoom (key height, row
//!   height), both around the pointer.

use eframe::egui::{self, Event, MouseWheelUnit, Vec2};

/// What the wheel did this frame (points; zoom as factors).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Wheel {
    /// Content moves right / down by this much.
    pub scroll: Vec2,
    pub zoom_x: f32,
    pub zoom_y: f32,
}

impl Wheel {
    pub fn none() -> Wheel {
        Wheel { scroll: Vec2::ZERO, zoom_x: 1.0, zoom_y: 1.0 }
    }

    pub fn is_none(&self) -> bool {
        self.scroll == Vec2::ZERO && self.zoom_x == 1.0 && self.zoom_y == 1.0
    }
}

/// Points per wheel line and page.
const LINE: f32 = 40.0;
const PAGE: f32 = 400.0;
/// Zoom per point of wheel (one notch of 40 points: x1.22).
const ZOOM_PER_POINT: f32 = 0.005;

/// The wheel events of a frame as scroll and zoom.
pub fn wheel(events: &[Event]) -> Wheel {
    let mut w = Wheel::none();
    for e in events {
        let Event::MouseWheel { unit, delta, modifiers, .. } = e else { continue };
        let d = match unit {
            MouseWheelUnit::Point => *delta,
            MouseWheelUnit::Line => *delta * LINE,
            MouseWheelUnit::Page => *delta * PAGE,
        };
        // one axis is enough for a zoom: whichever moved
        let amount = if d.y != 0.0 { d.y } else { d.x };
        if modifiers.ctrl || modifiers.command {
            w.zoom_x *= (amount * ZOOM_PER_POINT).exp();
        } else if modifiers.alt {
            w.zoom_y *= (amount * ZOOM_PER_POINT).exp();
        } else if modifiers.shift && d.x == 0.0 {
            w.scroll.x += d.y;
        } else {
            w.scroll += d;
        }
    }
    w
}

/// The frame's wheel, when the pointer is over `rect`.
pub fn wheel_over(ui: &egui::Ui, rect: egui::Rect) -> Wheel {
    ui.input(|i| match i.pointer.hover_pos() {
        Some(p) if rect.contains(p) => wheel(&i.events),
        _ => Wheel::none(),
    })
}

/// Zoom of a linear axis around an anchor: `offset` is the world position
/// at the axis's start, `scale` pixels per world unit, `anchor` the pointer
/// in pixels from the axis's start. The world position under the anchor
/// stays where it is. Returns (offset, scale).
pub fn zoom_about(offset: f32, scale: f32, anchor: f32, factor: f32, min: f32, max: f32) -> (f32, f32) {
    let at = offset + anchor / scale;
    let s = (scale * factor).clamp(min, max);
    (at - anchor / s, s)
}

/// The same for the piano roll's keys, which count downwards: `top` is the
/// key at the top edge, `row` pixels per key, `anchor` pixels below the top.
/// Returns (top, row).
pub fn zoom_keys_about(top: f32, row: f32, anchor: f32, factor: f32, min: f32, max: f32) -> (f32, f32) {
    let at = top - anchor / row;
    let r = (row * factor).clamp(min, max);
    (at + anchor / r, r)
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui::{Modifiers, TouchPhase, vec2};

    fn ev(dy: f32, m: Modifiers) -> Event {
        Event::MouseWheel { unit: MouseWheelUnit::Line, delta: vec2(0.0, dy), phase: TouchPhase::Move, source: egui::MouseWheelSource::Unknown, modifiers: m }
    }

    #[test]
    fn modifiers_pick_the_action() {
        let w = wheel(&[ev(1.0, Modifiers::NONE)]);
        assert_eq!(w.scroll, vec2(0.0, 40.0));
        assert_eq!((w.zoom_x, w.zoom_y), (1.0, 1.0));
        let w = wheel(&[ev(1.0, Modifiers::SHIFT)]);
        assert_eq!(w.scroll, vec2(40.0, 0.0));
        let w = wheel(&[ev(1.0, Modifiers::CTRL)]);
        assert!(w.zoom_x > 1.2 && w.zoom_x < 1.25 && w.zoom_y == 1.0 && w.scroll == Vec2::ZERO);
        let w = wheel(&[ev(-1.0, Modifiers::ALT)]);
        assert!(w.zoom_y < 1.0 && w.zoom_x == 1.0);
        // two notches in and out cancel
        let w = wheel(&[ev(2.0, Modifiers::CTRL), ev(-2.0, Modifiers::CTRL)]);
        assert!((w.zoom_x - 1.0).abs() < 1e-6);
        // a horizontal wheel with Shift stays horizontal
        let h = Event::MouseWheel { unit: MouseWheelUnit::Point, delta: vec2(30.0, 0.0), phase: TouchPhase::Move, source: egui::MouseWheelSource::Unknown, modifiers: Modifiers::SHIFT };
        assert_eq!(wheel(&[h]).scroll, vec2(30.0, 0.0));
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        // 0.5 px per tick from tick 100; the pointer at 200 px = tick 500
        let (o, s) = zoom_about(100.0, 0.5, 200.0, 2.0, 0.01, 16.0);
        assert_eq!(s, 1.0);
        assert!((o + 200.0 / s - 500.0).abs() < 1e-3);
        // clamped
        let (o, s) = zoom_about(100.0, 0.5, 200.0, 1000.0, 0.01, 16.0);
        assert_eq!(s, 16.0);
        assert!((o + 200.0 / s - 500.0).abs() < 1e-3);
        // keys: 8 px rows, top key 96, the pointer 80 px down = key 86
        let (t, r) = zoom_keys_about(96.0, 8.0, 80.0, 2.0, 3.0, 40.0);
        assert_eq!(r, 16.0);
        assert!((t - 80.0 / r - 86.0).abs() < 1e-4);
    }
}
