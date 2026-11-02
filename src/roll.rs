//! The piano roll: every channel's notes on the sequence's timeline (ticks,
//! 48 a quarter note), the loop markers in the ruler, the playhead, and the
//! edits of the active channel's notes (add, move, resize, delete,
//! velocity).

use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::seq::{Note, NoteSpec, PPQN};
use crate::session::Session;

const KEYS_W: f32 = 44.0;
const RULER_H: f32 = 24.0;
const VEL_H: f32 = 60.0;
const BAR: u32 = PPQN * 4;

/// A colour per channel.
pub fn channel_color(i: usize) -> Color32 {
    let h = (i as f32 * 0.618_034).fract();
    egui::ecolor::Hsva::new(h, 0.55, 0.95, 1.0).into()
}

pub fn key_name(k: u8) -> String {
    const N: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
    format!("{}{}", N[usize::from(k % 12)], i32::from(k) / 12 - 1)
}

/// Bar.beat (from 1) of a tick, in 4/4.
pub fn bar_beat(t: u32) -> String {
    format!("{}.{}", t / BAR + 1, t % BAR / PPQN + 1)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DragKind {
    Move { grab_ticks: i64, grab_key: i32 },
    Resize,
    Velocity,
    LoopStart,
    LoopEnd,
    Seek,
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    kind: DragKind,
    note: Option<Note>,
    /// The note (or tick) as the drag would leave it.
    spec: Option<NoteSpec>,
    tick: u32,
}

pub struct RollView {
    pub px_per_tick: f32,
    pub row_h: f32,
    pub scroll_tick: f32,
    /// The key of the top row.
    pub top_key: f32,
    /// Snap, in ticks.
    pub grid: u32,
    /// The channel being edited.
    pub active: usize,
    pub show_all: bool,
    pub follow: bool,
    /// (channel, note id)
    pub selected: Option<(usize, u32)>,
    pub new_len: u32,
    pub new_vel: u8,
    drag: Option<Drag>,
}

impl Default for RollView {
    fn default() -> Self {
        RollView {
            px_per_tick: 0.5,
            row_h: 8.0,
            scroll_tick: 0.0,
            top_key: 96.0,
            grid: PPQN / 4,
            active: 1,
            show_all: true,
            follow: true,
            selected: None,
            new_len: PPQN / 2,
            new_vel: 100,
            drag: None,
        }
    }
}

/// What the roll asks the app to do.
pub enum Action {
    /// The sequence changed (the note edits are already in the session).
    Edited,
    Seek(u32),
    Loop(u32, u32),
}

impl RollView {
    /// Fits the view to a newly loaded song.
    pub fn fit(&mut self, notes: &[Vec<Note>], width: f32) {
        let (mut lo, mut hi) = (127u8, 0u8);
        for n in notes.iter().flatten() {
            lo = lo.min(n.key);
            hi = hi.max(n.key);
        }
        if lo > hi {
            hi = 84;
        }
        self.top_key = (f32::from(hi) + 3.0).min(127.0);
        self.scroll_tick = 0.0;
        // about 16 bars across
        self.px_per_tick = ((width - KEYS_W) / (16.0 * BAR as f32)).clamp(0.02, 8.0);
        self.selected = None;
        self.drag = None;
        // the busiest channel
        self.active = (0..notes.len()).max_by_key(|&c| (notes[c].len(), std::cmp::Reverse(c))).unwrap_or(0);
    }

    fn snap(&self, t: f32) -> u32 {
        let g = self.grid.max(1) as f32;
        ((t.max(0.0) / g).round() * g) as u32
    }

    fn snap_floor(&self, t: f32) -> u32 {
        let g = self.grid.max(1) as f32;
        ((t.max(0.0) / g).floor() * g) as u32
    }

    pub fn show(&mut self, ui: &mut Ui, s: &mut Session, notes: &[Vec<Note>], playhead: Option<u32>) -> Vec<Action> {
        let mut actions = Vec::new();
        let (rect, resp) = ui.allocate_exact_size(ui.available_size().max(vec2(200.0, 200.0)), Sense::click_and_drag());
        let ruler = Rect::from_min_max(pos2(rect.left() + KEYS_W, rect.top()), pos2(rect.right(), rect.top() + RULER_H));
        let vel = Rect::from_min_max(pos2(rect.left() + KEYS_W, rect.bottom() - VEL_H), rect.right_bottom());
        let area = Rect::from_min_max(pos2(rect.left() + KEYS_W, ruler.bottom()), pos2(rect.right(), vel.top() - 2.0));
        let keys = Rect::from_min_max(pos2(rect.left(), area.top()), pos2(area.left(), area.bottom()));
        let end = s.seq.end_tick().max(1);
        let (la, lb) = s.markers();

        // scrolling and zoom
        if resp.hovered() {
            let (d, ctrl, hover) = ui.input(|i| (i.smooth_scroll_delta, i.modifiers.ctrl || i.modifiers.command, i.pointer.hover_pos()));
            if ctrl && d.y != 0.0 {
                let anchor = hover.map_or(area.center().x, |p| p.x);
                let t = self.scroll_tick + (anchor - area.left()) / self.px_per_tick;
                self.px_per_tick = (self.px_per_tick * (d.y / 200.0).exp()).clamp(0.01, 16.0);
                self.scroll_tick = t - (anchor - area.left()) / self.px_per_tick;
            } else {
                self.scroll_tick -= d.x / self.px_per_tick;
                self.top_key = (self.top_key + d.y / self.row_h).round();
            }
        }
        let rows = area.height() / self.row_h;
        self.top_key = self.top_key.clamp(rows.min(127.0), 127.0);
        let visible_ticks = area.width() / self.px_per_tick;
        if let (true, Some(p)) = (self.follow && self.drag.is_none(), playhead) {
            let p = p as f32;
            if p < self.scroll_tick || p > self.scroll_tick + visible_ticks * 0.92 {
                self.scroll_tick = p - visible_ticks * 0.05;
            }
        }
        self.scroll_tick = self.scroll_tick.clamp(-(BAR as f32), end as f32);

        let x_of = |t: f32| area.left() + (t - self.scroll_tick) * self.px_per_tick;
        let tick_of = |x: f32| self.scroll_tick + (x - area.left()) / self.px_per_tick;
        let y_of = |k: f32| area.top() + (self.top_key - k) * self.row_h;
        let key_of = |y: f32| (self.top_key - (y - area.top()) / self.row_h).ceil().clamp(0.0, 127.0) as u8;

        // pointer
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let press = ui.input(|i| i.pointer.press_origin());
        let active = self.active.min(notes.len().saturating_sub(1));
        let hit = |p: Pos2| -> Option<(Note, bool)> {
            if !area.contains(p) {
                return None;
            }
            let k = key_of(p.y);
            let t = tick_of(p.x);
            notes.get(active)?.iter().rev().find_map(|n| {
                let (x0, x1) = (x_of(n.start as f32), x_of(n.end() as f32).max(x_of(n.start as f32) + 3.0));
                (n.key == k && p.x >= x0 && p.x <= x1 && t >= n.start as f32 - 2.0).then_some((*n, p.x > x1 - 6.0))
            })
        };
        let vel_hit = |p: Pos2| -> Option<Note> {
            if !vel.contains(p) {
                return None;
            }
            notes.get(active)?.iter().rev().find(|n| {
                let x0 = x_of(n.start as f32);
                p.x >= x0 - 3.0 && p.x <= x0 + 4.0f32.max((n.len as f32 * self.px_per_tick).min(12.0))
            }).copied()
        };
        let marker_hit = |p: Pos2| -> Option<DragKind> {
            if !ruler.contains(p) {
                return None;
            }
            if (p.x - x_of(lb as f32)).abs() < 7.0 {
                Some(DragKind::LoopEnd)
            } else if (p.x - x_of(la as f32)).abs() < 7.0 {
                Some(DragKind::LoopStart)
            } else {
                None
            }
        };
        if let Some(p) = pointer {
            if self.drag.is_none() {
                if marker_hit(p).is_some() {
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
                } else if let Some((_, edge)) = hit(p) {
                    ui.ctx().set_cursor_icon(if edge { CursorIcon::ResizeHorizontal } else { CursorIcon::Grab });
                } else if vel_hit(p).is_some() {
                    ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
                }
            }
        }

        // drag start
        if resp.drag_started()
            && let Some(p) = press
        {
            self.drag = if let Some(k) = marker_hit(p) {
                Some(Drag { kind: k, note: None, spec: None, tick: if k == DragKind::LoopStart { la } else { lb } })
            } else if ruler.contains(p) {
                Some(Drag { kind: DragKind::Seek, note: None, spec: None, tick: self.snap(tick_of(p.x)) })
            } else if let Some(n) = vel_hit(p) {
                self.selected = Some((active, n.id));
                Some(Drag { kind: DragKind::Velocity, note: Some(n), spec: None, tick: 0 })
            } else if let Some((n, edge)) = hit(p) {
                self.selected = Some((active, n.id));
                let kind = if edge {
                    DragKind::Resize
                } else {
                    DragKind::Move { grab_ticks: (tick_of(p.x) - n.start as f32) as i64, grab_key: i32::from(key_of(p.y)) - i32::from(n.key) }
                };
                Some(Drag { kind, note: Some(n), spec: None, tick: 0 })
            } else {
                None
            };
        }
        // drag
        if let (Some(mut d), Some(p)) = (self.drag.take(), pointer.or(resp.interact_pointer_pos())) {
            let spec = d.note.map(|n| NoteSpec { start: n.start, len: n.len, key: n.key, vel: n.vel });
            match d.kind {
                DragKind::Move { grab_ticks, grab_key } => {
                    let n = d.note.expect("note");
                    let g = self.grid.max(1) as f32;
                    let t = ((tick_of(p.x) - grab_ticks as f32) / g).round() * g;
                    let k = (i32::from(key_of(p.y)) - grab_key).clamp(0, 127) as u8;
                    d.spec = Some(NoteSpec { start: t.max(0.0) as u32, len: n.len.max(1), key: k, vel: n.vel });
                }
                DragKind::Resize => {
                    let n = d.note.expect("note");
                    let e = self.snap(tick_of(p.x)).max(n.start + 1);
                    d.spec = Some(NoteSpec { len: e - n.start, ..spec.expect("note") });
                }
                DragKind::Velocity => {
                    let v = ((vel.bottom() - p.y) / vel.height() * 127.0).clamp(1.0, 127.0) as u8;
                    d.spec = Some(NoteSpec { vel: v, ..spec.expect("note") });
                }
                DragKind::LoopStart | DragKind::LoopEnd | DragKind::Seek => {
                    d.tick = self.snap(tick_of(p.x)).min(end + 1);
                }
            }
            self.drag = Some(d);
        }
        if resp.drag_stopped()
            && let Some(d) = self.drag.take()
        {
            match d.kind {
                DragKind::LoopStart => actions.push(Action::Loop(d.tick.min(lb.saturating_sub(1)), lb)),
                DragKind::LoopEnd => actions.push(Action::Loop(la, d.tick.max(la + 1))),
                DragKind::Seek => actions.push(Action::Seek(d.tick)),
                _ => {
                    if let (Some(n), Some(spec)) = (d.note, d.spec)
                        && (spec.start, spec.len, spec.key, spec.vel) != (n.start, n.len, n.key, n.vel)
                    {
                        s.seq.set_note(active, n.id, Some(spec));
                        self.new_len = spec.len;
                        self.new_vel = spec.vel;
                        actions.push(Action::Edited);
                    }
                }
            }
        }
        // clicks
        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            if ruler.contains(p) {
                actions.push(Action::Seek(self.snap_floor(tick_of(p.x))));
            } else if let Some((n, _)) = hit(p) {
                self.selected = Some((active, n.id));
            } else if area.contains(p) && active < s.seq.tracks.len() {
                let spec = NoteSpec { start: self.snap_floor(tick_of(p.x)), len: self.new_len.max(1), key: key_of(p.y), vel: self.new_vel };
                let id = s.seq.add_note(active, spec);
                self.selected = Some((active, id));
                actions.push(Action::Edited);
            }
        }
        if resp.secondary_clicked()
            && let Some(p) = resp.interact_pointer_pos()
            && let Some((n, _)) = hit(p)
        {
            s.seq.set_note(active, n.id, None);
            self.selected = None;
            actions.push(Action::Edited);
        }

        // painting
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(22));
        // key rows
        let lo_key = key_of(area.bottom() - 0.5);
        let hi_key = key_of(area.top() + 0.5);
        for k in lo_key..=hi_key {
            let y = y_of(f32::from(k));
            let black = [1, 3, 6, 8, 10].contains(&(k % 12));
            let r = Rect::from_min_max(pos2(area.left(), y), pos2(area.right(), y + self.row_h)).intersect(area);
            if black {
                painter.rect_filled(r, 0.0, Color32::from_gray(17));
            }
            if k % 12 == 0 {
                painter.hline(area.x_range(), y + self.row_h, Stroke::new(1.0, Color32::from_gray(48)));
            }
            let kr = Rect::from_min_max(pos2(keys.left(), y), pos2(keys.right() - 1.0, y + self.row_h)).intersect(keys);
            painter.rect_filled(kr, 0.0, if black { Color32::from_gray(40) } else { Color32::from_gray(200) });
            if k % 12 == 0 && self.row_h >= 6.0 {
                painter.text(pos2(keys.right() - 3.0, y + self.row_h / 2.0), Align2::RIGHT_CENTER, key_name(k), FontId::proportional(self.row_h.clamp(8.0, 12.0)), Color32::from_gray(30));
            }
        }
        // the loop range
        let (xa, xb) = (x_of(la as f32), x_of(lb as f32));
        let loop_color = if s.loop_range.is_some() { Color32::from_rgba_unmultiplied(80, 160, 255, 18) } else { Color32::from_rgba_unmultiplied(120, 200, 120, 14) };
        painter.rect_filled(Rect::from_min_max(pos2(xa.max(area.left()), area.top()), pos2(xb.min(area.right()), vel.bottom())), 0.0, loop_color);
        // grid
        let t0 = self.scroll_tick.max(0.0) as u32;
        let t1 = (self.scroll_tick + visible_ticks).max(0.0) as u32 + 1;
        let step = if PPQN as f32 * self.px_per_tick >= 8.0 { PPQN } else { BAR };
        let sub = (self.grid.max(1) as f32 * self.px_per_tick >= 10.0).then_some(self.grid.max(1));
        let first = t0 / step * step;
        let mut t = first;
        while t <= t1.min(end + BAR) {
            let x = x_of(t as f32);
            let c = if t % BAR == 0 { Color32::from_gray(70) } else { Color32::from_gray(42) };
            painter.vline(x, area.top()..=vel.bottom(), Stroke::new(1.0, c));
            if let Some(g) = sub {
                let mut u = t + g;
                while u < t + step {
                    painter.vline(x_of(u as f32), area.y_range(), Stroke::new(1.0, Color32::from_gray(30)));
                    u += g;
                }
            }
            t += step;
        }
        // the song's end
        let xe = x_of(end as f32);
        if xe < area.right() {
            painter.rect_filled(Rect::from_min_max(pos2(xe.max(area.left()), area.top()), pos2(area.right(), vel.bottom())), 0.0, Color32::from_black_alpha(90));
        }
        // notes
        let moving = self.drag.and_then(|d| d.note.map(|n| n.id));
        let draw = |n: &Note, ch: usize, strong: bool| {
            let (x0, x1) = (x_of(n.start as f32), x_of(n.end() as f32).max(x_of(n.start as f32) + 2.0));
            if x1 < area.left() || x0 > area.right() || n.key < lo_key || n.key > hi_key {
                return;
            }
            let y = y_of(f32::from(n.key));
            let r = Rect::from_min_max(pos2(x0, y + 0.5), pos2(x1, y + self.row_h - 0.5)).intersect(area);
            let c = channel_color(ch);
            if strong {
                // brightness by velocity
                let f = 0.45 + 0.55 * f32::from(n.vel) / 127.0;
                let c = Color32::from_rgb((f32::from(c.r()) * f) as u8, (f32::from(c.g()) * f) as u8, (f32::from(c.b()) * f) as u8);
                painter.rect_filled(r, 1.5, c);
                painter.rect_stroke(r, 1.5, Stroke::new(1.0, Color32::from_black_alpha(160)), StrokeKind::Inside);
            } else {
                painter.rect_filled(r, 1.0, c.gamma_multiply(0.3));
            }
        };
        if self.show_all {
            for (ch, ns) in notes.iter().enumerate() {
                if ch != active {
                    for n in ns {
                        draw(n, ch, false);
                    }
                }
            }
        }
        if let Some(ns) = notes.get(active) {
            for n in ns {
                if Some(n.id) == moving && self.drag.is_some_and(|d| d.spec.is_some()) {
                    continue;
                }
                draw(n, active, true);
            }
            if let Some((ch, id)) = self.selected
                && ch == active
                && let Some(n) = ns.iter().find(|n| n.id == id)
                && Some(n.id) != moving
            {
                let y = y_of(f32::from(n.key));
                let r = Rect::from_min_max(pos2(x_of(n.start as f32), y), pos2(x_of(n.end() as f32).max(x_of(n.start as f32) + 3.0), y + self.row_h));
                painter.rect_stroke(r.intersect(area.expand(1.0)), 1.5, Stroke::new(1.5, Color32::WHITE), StrokeKind::Outside);
            }
        }
        if let Some(Drag { spec: Some(sp), note: Some(n), .. }) = self.drag {
            let ghost = Note { start: sp.start, len: sp.len, key: sp.key, vel: sp.vel, ..n };
            draw(&ghost, active, true);
            let y = y_of(f32::from(sp.key));
            let r = Rect::from_min_max(pos2(x_of(sp.start as f32), y), pos2(x_of(ghost.end() as f32), y + self.row_h));
            painter.rect_stroke(r, 1.5, Stroke::new(1.5, Color32::WHITE), StrokeKind::Outside);
            painter.text(r.right_top() + vec2(4.0, -2.0), Align2::LEFT_BOTTOM, format!("{} {} len {} vel {}", key_name(sp.key), bar_beat(sp.start), sp.len, sp.vel), FontId::proportional(11.0), Color32::WHITE);
        }
        // velocity lane
        painter.rect_filled(vel, 0.0, Color32::from_gray(16));
        painter.text(pos2(rect.left() + 4.0, vel.center().y), Align2::LEFT_CENTER, "vel", FontId::proportional(11.0), Color32::from_gray(140));
        if let Some(ns) = notes.get(active) {
            let c = channel_color(active);
            for n in ns {
                let x = x_of(n.start as f32);
                if x < vel.left() - 4.0 || x > vel.right() {
                    continue;
                }
                let v = match self.drag {
                    Some(Drag { kind: DragKind::Velocity, note: Some(m), spec: Some(sp), .. }) if m.id == n.id => sp.vel,
                    _ => n.vel,
                };
                let h = f32::from(v) / 127.0 * (vel.height() - 4.0);
                let selected = self.selected == Some((active, n.id));
                painter.vline(x, (vel.bottom() - h)..=vel.bottom(), Stroke::new(if selected { 3.0 } else { 2.0 }, if selected { Color32::WHITE } else { c }));
            }
        }
        // ruler
        painter.rect_filled(ruler, 0.0, Color32::from_gray(34));
        let label_every = {
            let bar_px = BAR as f32 * self.px_per_tick;
            let mut e = 1u32;
            while bar_px * e as f32 <= 36.0 {
                e *= 2;
            }
            e
        };
        let mut b = t0 / BAR;
        while b * BAR <= t1.min(end + BAR) {
            if b % label_every == 0 {
                let x = x_of((b * BAR) as f32);
                painter.vline(x, ruler.y_range(), Stroke::new(1.0, Color32::from_gray(90)));
                painter.text(pos2(x + 3.0, ruler.top() + 2.0), Align2::LEFT_TOP, format!("{}", b + 1), FontId::proportional(11.0), Color32::from_gray(200));
            }
            b += 1;
        }
        let (da, db) = match self.drag {
            Some(Drag { kind: DragKind::LoopStart, tick, .. }) => (tick, lb),
            Some(Drag { kind: DragKind::LoopEnd, tick, .. }) => (la, tick),
            _ => (la, lb),
        };
        for (t, c, label) in [(da, Color32::from_rgb(90, 220, 120), "A"), (db, Color32::from_rgb(240, 110, 90), "B")] {
            let x = x_of(t as f32);
            if x >= ruler.left() - 8.0 && x <= ruler.right() + 8.0 {
                painter.vline(x, ruler.top()..=vel.bottom(), Stroke::new(1.5, c.gamma_multiply(0.8)));
                let tri = vec![pos2(x - 6.0, ruler.bottom() - 11.0), pos2(x + 6.0, ruler.bottom() - 11.0), pos2(x, ruler.bottom())];
                painter.add(egui::Shape::convex_polygon(tri, c, Stroke::NONE));
                painter.text(pos2(x, ruler.bottom() - 12.0), Align2::CENTER_BOTTOM, label, FontId::proportional(10.0), c);
            }
        }
        // playhead
        let head = match self.drag {
            Some(Drag { kind: DragKind::Seek, tick, .. }) => Some(tick),
            _ => playhead,
        };
        if let Some(p) = head {
            let x = x_of(p as f32);
            if x >= area.left() && x <= area.right() {
                painter.vline(x, ruler.top()..=vel.bottom(), Stroke::new(1.5, Color32::from_rgb(255, 230, 120)));
            }
        }
        painter.rect_filled(Rect::from_min_max(rect.left_top(), pos2(keys.right(), ruler.bottom())), 0.0, Color32::from_gray(34));
        painter.text(pos2(rect.left() + 4.0, rect.top() + 5.0), Align2::LEFT_TOP, "bar", FontId::proportional(11.0), Color32::from_gray(150));
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seq::Off;

    #[test]
    fn fit_picks_the_busiest_channel() {
        let n = |k| Note { id: 0, start: 0, len: 1, key: k, vel: 1, on: 0, off: Off::Open };
        let notes = vec![vec![], vec![n(60); 3], vec![n(40); 7], vec![n(70); 2]];
        let mut v = RollView::default();
        v.fit(&notes, 1000.0);
        assert_eq!(v.active, 2);
        assert_eq!(v.top_key, 73.0);
    }
}
