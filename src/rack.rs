//! The channel rack (the main view, after FL Studio's playlist): every
//! channel as a row, its header (number, instrument, an activity meter,
//! mute, solo, volume) on the left and its notes as a mini piano roll
//! across the whole song on the right, under one ruler with the playhead
//! and the loop markers. Double click a row: its piano roll.

use eframe::egui::{self, Align2, Color32, CursorIcon, FontId, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::roll::{bar_beat, channel_color, draw_markers, key_name, tooltip};
use crate::seq::{Note, PPQN};
use crate::session::Session;
use crate::view;

pub const HEADER_W: f32 = 270.0;
const RULER_H: f32 = 24.0;
const BAR: u32 = PPQN * 4;

#[derive(Debug, Clone, Copy, PartialEq)]
enum Drag {
    Seek(u32),
    LoopStart(u32),
    LoopEnd(u32),
}

pub struct RackView {
    pub px_per_tick: f32,
    pub scroll_tick: f32,
    pub row_h: f32,
    /// Pixels the rows are scrolled up.
    pub scroll_y: f32,
    pub follow: bool,
    /// The selected channel (the keyboard and the MIDI input play on it).
    pub selected: usize,
    drag: Option<Drag>,
    fit_pending: bool,
}

impl Default for RackView {
    fn default() -> Self {
        RackView { px_per_tick: 0.1, scroll_tick: 0.0, row_h: 34.0, scroll_y: 0.0, follow: true, selected: 1, drag: None, fit_pending: true }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum RackAction {
    Seek(u32),
    Loop(u32, u32),
    /// Open the channel's piano roll.
    Open(usize),
}

pub struct RackShow<'a> {
    pub notes: &'a [Vec<Note>],
    pub playhead: Option<u32>,
    /// Per channel how loud it is now, 0..1.
    pub levels: &'a [f32],
    /// Per channel the instrument it plays (now, or its first).
    pub programs: &'a [Option<usize>],
}

/// A level (0..1; one loud note is about 0.15) as a meter's fill, 0..1.
pub fn meter(level: f32) -> f32 {
    (level.max(0.0) * 4.0).sqrt().min(1.0)
}

impl RackView {
    /// The whole song across the width (on the next frame).
    pub fn fit(&mut self) {
        self.fit_pending = true;
        self.scroll_y = 0.0;
    }

    pub fn show(&mut self, ui: &mut Ui, s: &mut Session, v: RackShow<'_>) -> Vec<RackAction> {
        let mut actions = Vec::new();
        let n = s.seq.tracks.len();
        let (rect, resp) = ui.allocate_exact_size(ui.available_size().max(vec2(400.0, 160.0)), Sense::click_and_drag());
        let ruler = Rect::from_min_max(pos2(rect.left() + HEADER_W, rect.top()), pos2(rect.right(), rect.top() + RULER_H));
        let lanes = Rect::from_min_max(pos2(ruler.left(), ruler.bottom()), rect.right_bottom());
        let heads = Rect::from_min_max(pos2(rect.left(), ruler.bottom()), pos2(ruler.left(), rect.bottom()));
        let end = s.seq.end_tick().max(1);
        let (la, lb) = s.markers();
        if self.fit_pending {
            self.px_per_tick = (lanes.width() / (end + BAR / 2) as f32).clamp(0.005, 16.0);
            self.scroll_tick = 0.0;
            self.fit_pending = false;
        }

        // wheel: rows; Shift: time; Ctrl: zoom the time; Alt: row height
        let w = view::wheel_over(ui, rect);
        if !w.is_none() {
            let hover = ui.input(|i| i.pointer.hover_pos()).unwrap_or(lanes.center());
            if w.zoom_x != 1.0 {
                (self.scroll_tick, self.px_per_tick) = view::zoom_about(self.scroll_tick, self.px_per_tick, (hover.x - lanes.left()).max(0.0), w.zoom_x, 0.005, 16.0);
            }
            if w.zoom_y != 1.0 {
                let anchor = (hover.y - lanes.top()).max(0.0);
                let (rows, h) = view::zoom_about(self.scroll_y / self.row_h, self.row_h, anchor, w.zoom_y, 16.0, 120.0);
                self.row_h = h;
                self.scroll_y = rows * h;
            }
            self.scroll_tick -= w.scroll.x / self.px_per_tick;
            self.scroll_y -= w.scroll.y;
        }
        let content_h = n as f32 * self.row_h;
        self.scroll_y = self.scroll_y.clamp(0.0, (content_h - lanes.height() + 8.0).max(0.0));
        let visible_ticks = lanes.width() / self.px_per_tick;
        if let (true, Some(p)) = (self.follow && self.drag.is_none(), v.playhead) {
            let p = p as f32;
            if p < self.scroll_tick || p > self.scroll_tick + visible_ticks * 0.95 {
                self.scroll_tick = p - visible_ticks * 0.05;
            }
        }
        self.scroll_tick = self.scroll_tick.clamp(-(BAR as f32), end as f32);
        let x_of = |t: f32| lanes.left() + (t - self.scroll_tick) * self.px_per_tick;
        let tick_of = |x: f32| self.scroll_tick + (x - lanes.left()) / self.px_per_tick;
        let row_top = |c: usize| lanes.top() + c as f32 * self.row_h - self.scroll_y;
        let row_of = |y: f32| -> Option<usize> {
            if y < lanes.top() {
                return None;
            }
            let r = ((y - lanes.top() + self.scroll_y) / self.row_h).floor();
            (r >= 0.0 && (r as usize) < n).then_some(r as usize)
        };
        let snap = |t: f32| (t.max(0.0) / PPQN as f32).round() as u32 * PPQN;

        // the ruler: seek (click or drag), the loop markers
        let pointer = ui.input(|i| i.pointer.hover_pos());
        let press = ui.input(|i| i.pointer.press_origin());
        let marker = |x: f32| -> Option<Drag> {
            if (x - x_of(lb as f32)).abs() < 7.0 {
                Some(Drag::LoopEnd(lb))
            } else if (x - x_of(la as f32)).abs() < 7.0 {
                Some(Drag::LoopStart(la))
            } else {
                None
            }
        };
        if let Some(p) = pointer
            && ruler.contains(p)
            && marker(p.x).is_some()
        {
            ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
        }
        if resp.drag_started()
            && let Some(p) = press
            && ruler.contains(p)
        {
            self.drag = Some(marker(p.x).unwrap_or(Drag::Seek(snap(tick_of(p.x)))));
        }
        if let (Some(d), Some(p)) = (self.drag, pointer) {
            let t = snap(tick_of(p.x)).min(end + 1);
            self.drag = Some(match d {
                Drag::Seek(_) => Drag::Seek(t),
                Drag::LoopStart(_) => Drag::LoopStart(t),
                Drag::LoopEnd(_) => Drag::LoopEnd(t),
            });
        }
        if resp.drag_stopped()
            && let Some(d) = self.drag.take()
        {
            match d {
                Drag::Seek(t) => actions.push(RackAction::Seek(t)),
                Drag::LoopStart(t) => actions.push(RackAction::Loop(t.min(lb.saturating_sub(1)), lb)),
                Drag::LoopEnd(t) => actions.push(RackAction::Loop(la, t.max(la + 1))),
            }
        }
        if let Some(p) = resp.interact_pointer_pos() {
            if resp.clicked() && ruler.contains(p) {
                actions.push(RackAction::Seek((tick_of(p.x).max(0.0) as u32).min(end)));
            } else if (resp.clicked() || resp.double_clicked()) && (lanes.contains(p) || heads.contains(p))
                && let Some(c) = row_of(p.y)
            {
                self.selected = c;
                if resp.double_clicked() {
                    actions.push(RackAction::Open(c));
                }
            }
        }

        // painting
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(20));
        let lp = painter.with_clip_rect(lanes);
        let hp = painter.with_clip_rect(heads);
        let t0 = self.scroll_tick.max(0.0) as u32;
        let t1 = (self.scroll_tick + visible_ticks).max(0.0) as u32 + 1;
        // bar lines (as many as fit)
        let mut every = 1u32;
        while BAR as f32 * self.px_per_tick * every as f32 <= 6.0 {
            every *= 2;
        }
        let mut b = t0 / BAR / every * every;
        while b * BAR <= t1.min(end + BAR) {
            let x = x_of((b * BAR) as f32);
            lp.vline(x, lanes.y_range(), Stroke::new(1.0, if b % (every * 4) == 0 { Color32::from_gray(52) } else { Color32::from_gray(36) }));
            b += every;
        }
        let (xa, xb) = (x_of(la as f32), x_of(lb as f32));
        let loop_color = if s.loop_range.is_some() { Color32::from_rgba_unmultiplied(80, 160, 255, 16) } else { Color32::from_rgba_unmultiplied(120, 200, 120, 12) };
        lp.rect_filled(Rect::from_min_max(pos2(xa.max(lanes.left()), lanes.top()), pos2(xb.min(lanes.right()), lanes.bottom())), 0.0, loop_color);
        let xe = x_of(end as f32);
        if xe < lanes.right() {
            lp.rect_filled(Rect::from_min_max(pos2(xe.max(lanes.left()), lanes.top()), lanes.right_bottom()), 0.0, Color32::from_black_alpha(90));
        }
        let solo = s.channels.iter().any(|c| c.solo);
        let mut hover_tip = None;
        for c in 0..n {
            let top = row_top(c);
            if top > rect.bottom() || top + self.row_h < lanes.top() {
                continue;
            }
            let lane = Rect::from_min_max(pos2(lanes.left(), top), pos2(lanes.right(), top + self.row_h));
            let head = Rect::from_min_max(pos2(heads.left(), top), pos2(heads.right(), top + self.row_h));
            let col = channel_color(c);
            let heard = !(s.channels[c].mute || (solo && !s.channels[c].solo));
            let sel = c == self.selected;
            // row backgrounds
            let bg = if sel { Color32::from_rgb(40, 44, 56) } else if c % 2 == 0 { Color32::from_gray(26) } else { Color32::from_gray(23) };
            hp.rect_filled(head, 0.0, bg);
            lp.rect_filled(lane, 0.0, bg.gamma_multiply(0.85));
            lp.hline(lane.x_range(), lane.bottom() - 0.5, Stroke::new(1.0, Color32::from_gray(14)));
            hp.hline(head.x_range(), head.bottom() - 0.5, Stroke::new(1.0, Color32::from_gray(14)));
            hp.rect_filled(Rect::from_min_size(head.min, vec2(5.0, self.row_h - 1.0)), 0.0, col);
            // the activity meter: a LED and a bar
            let level = v.levels.get(c).copied().unwrap_or(0.0);
            let m = meter(level);
            let led = pos2(head.left() + 16.0, head.center().y);
            hp.circle_filled(led, 5.0, Color32::from_gray(45));
            if m > 0.02 {
                hp.circle_filled(led, 5.0, col.gamma_multiply(0.35 + 0.65 * m));
                hp.circle_stroke(led, 6.0, Stroke::new(1.0, col.gamma_multiply(m)));
            }
            let bar = Rect::from_min_max(pos2(head.left() + 26.0, head.top() + 4.0), pos2(head.left() + 30.0, head.bottom() - 4.0));
            hp.rect_filled(bar, 1.0, Color32::from_gray(40));
            let mut fill = bar;
            fill.set_top(bar.bottom() - bar.height() * m);
            hp.rect_filled(fill, 1.0, if m > 0.95 { Color32::from_rgb(255, 120, 90) } else { col });
            // number and instrument
            let notes = v.notes.get(c).map_or(0, Vec::len);
            let name_c = if heard { Color32::from_gray(230) } else { Color32::from_gray(110) };
            hp.text(pos2(head.left() + 36.0, head.center().y), Align2::LEFT_CENTER, format!("{c}"), FontId::monospace(14.0), name_c);
            let inst = match (s.channels[c].instrument, v.programs.get(c).copied().flatten()) {
                (Some((b, i, _)), _) => format!("bank {b} #{i}"),
                (None, Some(p)) => format!("{} #{p}", s.wd_orig.id),
                (None, None) => "-".into(),
            };
            let sub = if notes == 0 { "no notes".to_string() } else { format!("{notes} notes") };
            if self.row_h >= 30.0 {
                hp.text(pos2(head.left() + 60.0, head.center().y - 7.0), Align2::LEFT_CENTER, inst, FontId::proportional(12.0), name_c);
                hp.text(pos2(head.left() + 60.0, head.center().y + 7.0), Align2::LEFT_CENTER, sub, FontId::proportional(10.5), Color32::from_gray(120));
            } else {
                hp.text(pos2(head.left() + 60.0, head.center().y), Align2::LEFT_CENTER, format!("{inst} \u{b7} {sub}"), FontId::proportional(11.0), name_c);
            }
            // the mini roll: the channel's key range over the row's height
            let ns = v.notes.get(c).map_or(&[][..], Vec::as_slice);
            if !ns.is_empty() {
                let lo = ns.iter().map(|n| n.key).min().unwrap_or(0);
                let hi = ns.iter().map(|n| n.key).max().unwrap_or(127);
                let span = f32::from(hi - lo) + 1.0;
                let inner = lane.shrink2(vec2(0.0, 3.0));
                let kh = (inner.height() / span).clamp(1.5, 6.0);
                let y_of = |k: u8| inner.bottom() - (f32::from(k - lo) + 0.5) / span * inner.height();
                let nc = if heard { col } else { col.gamma_multiply(0.35) };
                for nt in ns {
                    let (x0, x1) = (x_of(nt.start as f32), x_of(nt.end() as f32));
                    if x1 < lanes.left() || x0 > lanes.right() {
                        continue;
                    }
                    let y = y_of(nt.key);
                    let r = Rect::from_min_max(pos2(x0, y - kh / 2.0), pos2(x1.max(x0 + 1.5), y + kh / 2.0));
                    lp.rect_filled(r, 0.5, nc.gamma_multiply(0.45 + 0.55 * f32::from(nt.vel) / 127.0));
                }
                if let Some(p) = pointer
                    && lane.contains(p)
                    && self.drag.is_none()
                {
                    let t = tick_of(p.x);
                    let at: Vec<&Note> = ns.iter().filter(|n| (n.start as f32) <= t && (n.end() as f32) >= t).collect();
                    let keys: Vec<String> = at.iter().take(6).map(|n| key_name(n.key)).collect();
                    hover_tip = Some(format!(
                        "channel {c}: {notes} notes, {}..{}\nbar {}{}\ndouble click: piano roll",
                        key_name(lo),
                        key_name(hi),
                        bar_beat(t.max(0.0) as u32),
                        if keys.is_empty() { String::new() } else { format!("  sounding: {}", keys.join(" ")) }
                    ));
                }
            }
            if sel {
                lp.rect_stroke(lane, 0.0, Stroke::new(1.0, Color32::from_rgb(110, 130, 190)), StrokeKind::Inside);
            }
        }
        // mute, solo, volume: widgets on the header rows
        let mut hui = ui.new_child(egui::UiBuilder::new().max_rect(heads));
        hui.set_clip_rect(heads.intersect(ui.clip_rect()));
        for c in 0..n {
            let top = row_top(c);
            if top > rect.bottom() || top + self.row_h < lanes.top() {
                continue;
            }
            let cy = top + self.row_h / 2.0;
            let bh = 18.0f32.min(self.row_h - 4.0);
            let r_m = Rect::from_center_size(pos2(heads.right() - 118.0, cy), vec2(22.0, bh));
            let r_s = Rect::from_center_size(pos2(heads.right() - 92.0, cy), vec2(22.0, bh));
            let r_v = Rect::from_min_max(pos2(heads.right() - 76.0, cy - bh / 2.0), pos2(heads.right() - 6.0, cy + bh / 2.0));
            let ch = &mut s.channels[c];
            let mute = ch.mute;
            if hui.put(r_m, egui::Button::new(egui::RichText::new("M").size(11.0)).selected(mute)).on_hover_text("Mute").clicked() {
                ch.mute = !mute;
            }
            let solo = ch.solo;
            if hui.put(r_s, egui::Button::new(egui::RichText::new("S").size(11.0)).selected(solo)).on_hover_text("Solo").clicked() {
                ch.solo = !solo;
            }
            hui.scope_builder(egui::UiBuilder::new().max_rect(r_v), |ui| {
                ui.spacing_mut().slider_width = r_v.width() - 4.0;
                ui.add(egui::Slider::new(&mut ch.gain, 0.0..=2.0).show_value(false)).on_hover_text(format!("Volume x{:.2}", ch.gain));
            });
        }
        // ruler
        painter.rect_filled(ruler, 0.0, Color32::from_gray(34));
        let mut every = 1u32;
        while BAR as f32 * self.px_per_tick * every as f32 <= 36.0 {
            every *= 2;
        }
        let mut b = t0 / BAR / every * every;
        while b * BAR <= t1.min(end + BAR) {
            let x = x_of((b * BAR) as f32);
            if x >= ruler.left() {
                painter.vline(x, ruler.y_range(), Stroke::new(1.0, Color32::from_gray(90)));
                painter.text(pos2(x + 3.0, ruler.top() + 2.0), Align2::LEFT_TOP, format!("{}", b + 1), FontId::proportional(11.0), Color32::from_gray(200));
            }
            b += every;
        }
        let (da, db) = match self.drag {
            Some(Drag::LoopStart(t)) => (t, lb),
            Some(Drag::LoopEnd(t)) => (la, t),
            _ => (la, lb),
        };
        draw_markers(&painter.with_clip_rect(Rect::from_min_max(ruler.min, lanes.max)), ruler, lanes.bottom(), x_of(da as f32), x_of(db as f32));
        let head = match self.drag {
            Some(Drag::Seek(t)) => Some(t),
            _ => v.playhead,
        };
        if let Some(p) = head {
            let x = x_of(p as f32);
            if x >= lanes.left() && x <= lanes.right() {
                painter.vline(x, ruler.top()..=lanes.bottom(), Stroke::new(1.5, Color32::from_rgb(255, 230, 120)));
            }
        }
        let corner = Rect::from_min_max(rect.left_top(), pos2(heads.right(), ruler.bottom()));
        painter.rect_filled(corner, 0.0, Color32::from_gray(34));
        painter.text(pos2(corner.left() + 8.0, corner.center().y), Align2::LEFT_CENTER, "channel \u{b7} instrument", FontId::proportional(11.0), Color32::from_gray(160));
        painter.text(pos2(heads.right() - 118.0, corner.center().y), Align2::CENTER_CENTER, "M", FontId::proportional(11.0), Color32::from_gray(160));
        painter.text(pos2(heads.right() - 92.0, corner.center().y), Align2::CENTER_CENTER, "S", FontId::proportional(11.0), Color32::from_gray(160));
        painter.text(pos2(heads.right() - 41.0, corner.center().y), Align2::CENTER_CENTER, "volume", FontId::proportional(11.0), Color32::from_gray(160));
        if let (Some(t), Some(p)) = (hover_tip, pointer) {
            tooltip(&painter, rect, p, &t);
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_scale() {
        assert_eq!(meter(0.0), 0.0);
        assert!((meter(0.25) - 1.0).abs() < 1e-6);
        assert!(meter(0.01) > 0.15 && meter(0.01) < 0.25);
        assert_eq!(meter(3.0), 1.0);
        assert_eq!(meter(-1.0), 0.0);
    }
}
