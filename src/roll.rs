//! The piano roll: the notes on the sequence's timeline (ticks, 48 a
//! quarter note), the loop markers in the ruler, the playhead, and the
//! edits of the active channel's notes: add, select (click, Ctrl/Shift+
//! click, box), move and resize a selection, delete, copy and paste, nudge
//! with the arrow keys, velocity. The keyboard strip on the left plays the
//! channel's instrument while a key is held.

use eframe::egui::{self, Align2, Color32, CursorIcon, Event, FontId, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::edit::{self, Selection};
use crate::seq::{Note, NoteSpec, PPQN};
use crate::session::Session;
use crate::view;

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

pub fn grid_name(g: u32) -> String {
    match g {
        192 => "1 bar".into(),
        48 => "1/4".into(),
        24 => "1/8".into(),
        12 => "1/16".into(),
        6 => "1/32".into(),
        16 => "1/8T".into(),
        8 => "1/16T".into(),
        1 => "tick".into(),
        g => format!("{g}"),
    }
}

/// A note's length in words ("1/8", "3 x 1/16", "29 ticks").
fn len_name(len: u32) -> String {
    for g in [BAR, PPQN, PPQN / 2, PPQN / 4, PPQN / 8] {
        if len % g == 0 {
            let n = len / g;
            return if n == 1 { grid_name(g) } else { format!("{n} x {}", grid_name(g)) };
        }
    }
    format!("{len} ticks")
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum DragKind {
    /// The selection moves; the grabbed note's offsets from the pointer.
    Move { grab_ticks: f32, grab_key: i32 },
    Resize,
    Velocity,
    /// A selection box from (tick, key); `add`: Ctrl/Shift held.
    Box { tick: f32, key: u8, add: bool },
    LoopStart,
    LoopEnd,
    Seek,
    /// The keyboard strip (the key plays while held).
    Keys,
}

#[derive(Debug, Clone, Copy)]
struct Drag {
    kind: DragKind,
    /// The note grabbed (move, resize, velocity).
    note: Option<Note>,
    /// Move: (ticks, keys) so far; resize: (ticks, 0).
    delta: (i64, i32),
    /// Velocity being set; box: the current corner (tick, key); markers
    /// and seek: the tick.
    vel: u8,
    corner: (f32, u8),
    tick: u32,
}

impl Drag {
    fn new(kind: DragKind, note: Option<Note>) -> Drag {
        Drag { kind, note, delta: (0, 0), vel: note.map_or(0, |n| n.vel), corner: (0.0, 0), tick: 0 }
    }
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
    /// Selected notes of the active channel.
    pub selected: Selection,
    sel_channel: usize,
    pub new_len: u32,
    pub new_vel: u8,
    drag: Option<Drag>,
    /// The key held on the keyboard strip; a short click's key-off time.
    held_key: Option<u8>,
    click_off: Option<(u8, f64)>,
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
            selected: Selection::new(),
            sel_channel: 1,
            new_len: PPQN / 2,
            new_vel: 100,
            drag: None,
            held_key: None,
            click_off: None,
        }
    }
}

/// What the roll asks the app to do.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// The sequence changed (the note edits are already in the session).
    Edited,
    Seek(u32),
    Loop(u32, u32),
    /// A key of the keyboard strip went down / up.
    KeyOn(u8),
    KeyOff(u8),
}

/// What the roll shows besides the session.
pub struct Show<'a> {
    pub notes: &'a [Vec<Note>],
    pub playhead: Option<u32>,
    /// Keys sounding live on the active channel (lit on the strip).
    pub lit: &'a [u8],
    /// Notes being recorded (shown, not yet in the sequence).
    pub ghosts: &'a [NoteSpec],
}

impl RollView {
    /// Fits the view to a newly loaded song (`keep_zoom`: the zoom from
    /// the settings).
    pub fn fit(&mut self, notes: &[Vec<Note>], width: f32, keep_zoom: bool) {
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
        if !keep_zoom {
            // about 16 bars across
            self.px_per_tick = ((width - KEYS_W) / (16.0 * BAR as f32)).clamp(0.02, 8.0);
        }
        self.selected.clear();
        self.drag = None;
        // the busiest channel
        self.active = (0..notes.len()).max_by_key(|&c| (notes[c].len(), std::cmp::Reverse(c))).unwrap_or(0);
    }

    /// Shows channel `c` alone (the rack's double click).
    pub fn focus(&mut self, c: usize) {
        self.active = c;
        self.show_all = false;
    }

    /// The selection, if it belongs to channel `c`.
    fn selection_of(&mut self, c: usize) -> &mut Selection {
        if self.sel_channel != c {
            self.selected.clear();
            self.sel_channel = c;
        }
        &mut self.selected
    }

    pub fn show(&mut self, ui: &mut Ui, s: &mut Session, v: Show<'_>) -> Vec<Action> {
        let notes = v.notes;
        let playhead = v.playhead;
        let mut actions = Vec::new();
        let (rect, resp) = ui.allocate_exact_size(ui.available_size().max(vec2(200.0, 200.0)), Sense::click_and_drag());
        let ruler = Rect::from_min_max(pos2(rect.left() + KEYS_W, rect.top()), pos2(rect.right(), rect.top() + RULER_H));
        let vel = Rect::from_min_max(pos2(rect.left() + KEYS_W, rect.bottom() - VEL_H), rect.right_bottom());
        let area = Rect::from_min_max(pos2(rect.left() + KEYS_W, ruler.bottom()), pos2(rect.right(), vel.top() - 2.0));
        let keys = Rect::from_min_max(pos2(rect.left(), area.top()), pos2(area.left(), area.bottom()));
        let end = s.seq.end_tick().max(1);
        let (la, lb) = s.markers();
        let active = self.active.min(notes.len().saturating_sub(1));
        self.selection_of(active);
        let none: Vec<Note> = Vec::new();
        let chan = notes.get(active).unwrap_or(&none);
        // notes deleted elsewhere (undo) leave the selection
        self.selected.retain(|id| chan.iter().any(|n| n.id == *id));

        // scrolling and zoom
        let w = view::wheel_over(ui, rect);
        if !w.is_none() {
            let hover = ui.input(|i| i.pointer.hover_pos()).unwrap_or(area.center());
            if w.zoom_x != 1.0 {
                (self.scroll_tick, self.px_per_tick) = view::zoom_about(self.scroll_tick, self.px_per_tick, hover.x - area.left(), w.zoom_x, 0.01, 16.0);
            }
            if w.zoom_y != 1.0 {
                (self.top_key, self.row_h) = view::zoom_keys_about(self.top_key, self.row_h, hover.y - area.top(), w.zoom_y, 3.0, 40.0);
            }
            self.scroll_tick -= w.scroll.x / self.px_per_tick;
            self.top_key += w.scroll.y / self.row_h;
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
        let mods = ui.input(|i| i.modifiers);
        let hit = |p: Pos2| -> Option<(Note, bool)> {
            if !area.contains(p) {
                return None;
            }
            let k = key_of(p.y);
            let t = tick_of(p.x);
            chan.iter().rev().find_map(|n| {
                let (x0, x1) = (x_of(n.start as f32), x_of(n.end() as f32).max(x_of(n.start as f32) + 3.0));
                (n.key == k && p.x >= x0 && p.x <= x1 && t >= n.start as f32 - 2.0).then_some((*n, p.x > x1 - 6.0))
            })
        };
        let vel_hit = |p: Pos2| -> Option<Note> {
            if !vel.contains(p) {
                return None;
            }
            chan.iter()
                .rev()
                .find(|n| {
                    let x0 = x_of(n.start as f32);
                    p.x >= x0 - 3.0 && p.x <= x0 + 4.0f32.max((n.len as f32 * self.px_per_tick).min(12.0))
                })
                .copied()
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
        if let Some(p) = pointer
            && self.drag.is_none()
            && rect.contains(p)
        {
            if marker_hit(p).is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeHorizontal);
            } else if let Some((_, edge)) = hit(p) {
                ui.ctx().set_cursor_icon(if edge { CursorIcon::ResizeHorizontal } else { CursorIcon::Grab });
            } else if vel_hit(p).is_some() {
                ui.ctx().set_cursor_icon(CursorIcon::ResizeVertical);
            } else if keys.contains(p) {
                ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
            }
        }

        // the keyboard strip: a key sounds while the button is held on it
        let down = ui.input(|i| i.pointer.primary_down());
        let now = ui.input(|i| i.time);
        let on_keys = down && press.is_some_and(|p| keys.contains(p)) && resp.is_pointer_button_down_on();
        let want = if on_keys { pointer.filter(|p| keys.y_range().contains(p.y)).map(|p| key_of(p.y)) } else { None };
        if want != self.held_key {
            if let Some(k) = self.held_key {
                actions.push(Action::KeyOff(k));
            }
            if let Some(k) = want {
                actions.push(Action::KeyOn(k));
                self.click_off = None;
            }
            self.held_key = want;
        }
        if let Some((k, t)) = self.click_off
            && now >= t
        {
            actions.push(Action::KeyOff(k));
            self.click_off = None;
        }

        // drag start
        if resp.drag_started()
            && let Some(p) = press
        {
            self.drag = if keys.contains(p) {
                Some(Drag::new(DragKind::Keys, None))
            } else if let Some(k) = marker_hit(p) {
                Some(Drag { tick: if k == DragKind::LoopStart { la } else { lb }, ..Drag::new(k, None) })
            } else if ruler.contains(p) {
                Some(Drag { tick: self.snap(tick_of(p.x)), ..Drag::new(DragKind::Seek, None) })
            } else if let Some(n) = vel_hit(p) {
                Some(Drag::new(DragKind::Velocity, Some(n)))
            } else if let Some((n, edge)) = hit(p) {
                if !self.selected.contains(&n.id) {
                    if !(mods.ctrl || mods.shift) {
                        self.selected.clear();
                    }
                    self.selected.insert(n.id);
                }
                let kind = if edge {
                    DragKind::Resize
                } else {
                    DragKind::Move { grab_ticks: tick_of(p.x) - n.start as f32, grab_key: i32::from(key_of(p.y)) - i32::from(n.key) }
                };
                Some(Drag::new(kind, Some(n)))
            } else if area.contains(p) {
                Some(Drag { corner: (tick_of(p.x), key_of(p.y)), ..Drag::new(DragKind::Box { tick: tick_of(p.x), key: key_of(p.y), add: mods.ctrl || mods.shift }, None) })
            } else {
                None
            };
        }
        // drag
        if let (Some(mut d), Some(p)) = (self.drag.take(), pointer.or(resp.interact_pointer_pos())) {
            match d.kind {
                DragKind::Move { grab_ticks, grab_key } => {
                    let n = d.note.expect("note");
                    let t = edit::snap(tick_of(p.x) - grab_ticks, self.grid);
                    let k = i32::from(key_of(p.y)) - grab_key;
                    d.delta = (i64::from(t) - i64::from(n.start), k - i32::from(n.key));
                }
                DragKind::Resize => {
                    let n = d.note.expect("note");
                    let e = edit::snap(tick_of(p.x), self.grid).max(n.start + 1);
                    d.delta = (i64::from(e) - i64::from(n.end()), 0);
                }
                DragKind::Velocity => {
                    d.vel = ((vel.bottom() - p.y) / vel.height() * 127.0).clamp(1.0, 127.0) as u8;
                }
                DragKind::Box { .. } => d.corner = (tick_of(p.x), key_of(p.y.clamp(area.top(), area.bottom() - 1.0))),
                DragKind::LoopStart | DragKind::LoopEnd | DragKind::Seek => {
                    d.tick = self.snap(tick_of(p.x)).min(end + 1);
                }
                DragKind::Keys => {}
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
                DragKind::Box { tick, key, add } => {
                    let (t1, k1) = d.corner;
                    let b = edit::in_box(chan, tick.max(0.0) as u32, t1.max(0.0) as u32, key, k1);
                    if !add {
                        self.selected.clear();
                    }
                    self.selected.extend(b);
                }
                DragKind::Move { .. } | DragKind::Resize if d.delta != (0, 0) => {
                    let changes = if matches!(d.kind, DragKind::Resize) {
                        edit::resized(chan, &self.selected, d.delta.0)
                    } else {
                        edit::moved(chan, &self.selected, d.delta.0, d.delta.1)
                    };
                    for (id, spec) in &changes {
                        s.seq.set_note(active, *id, Some(*spec));
                    }
                    if let Some((_, sp)) = changes.iter().find(|c| Some(c.0) == d.note.map(|n| n.id)) {
                        self.new_len = sp.len;
                    }
                    actions.push(Action::Edited);
                }
                DragKind::Velocity => {
                    if let Some(n) = d.note
                        && d.vel != n.vel
                    {
                        s.seq.set_note(active, n.id, Some(NoteSpec { vel: d.vel, ..edit::spec(&n) }));
                        self.selected = [n.id].into();
                        self.new_vel = d.vel;
                        actions.push(Action::Edited);
                    }
                }
                _ => {}
            }
        }
        // clicks
        if resp.clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            if keys.contains(p) {
                // a click too short to be seen held: a short note
                if self.held_key.is_none() {
                    let k = key_of(p.y);
                    actions.push(Action::KeyOn(k));
                    self.click_off = Some((k, now + 0.25));
                }
            } else if ruler.contains(p) {
                actions.push(Action::Seek(self.snap_floor(tick_of(p.x))));
            } else if let Some((n, _)) = hit(p) {
                if mods.ctrl || mods.shift {
                    if !self.selected.remove(&n.id) {
                        self.selected.insert(n.id);
                    }
                } else {
                    self.selected = [n.id].into();
                    self.new_len = n.len;
                    self.new_vel = n.vel;
                }
            } else if let Some(n) = vel_hit(p) {
                self.selected = [n.id].into();
            } else if area.contains(p) && active < s.seq.tracks.len() {
                if mods.ctrl || mods.shift {
                    // Ctrl/Shift+click on empty space only changes nothing
                } else {
                    let spec = NoteSpec { start: self.snap_floor(tick_of(p.x)), len: self.new_len.max(1), key: key_of(p.y), vel: self.new_vel };
                    let id = s.seq.add_note(active, spec);
                    self.selected = [id].into();
                    actions.push(Action::Edited);
                    // hear it
                    actions.push(Action::KeyOn(spec.key));
                    self.click_off = Some((spec.key, now + 0.25));
                }
            }
        }
        if resp.secondary_clicked()
            && let Some(p) = resp.interact_pointer_pos()
        {
            if let Some((n, _)) = hit(p) {
                s.seq.set_note(active, n.id, None);
                self.selected.remove(&n.id);
                actions.push(Action::Edited);
            } else {
                self.selected.clear();
            }
        }

        // keyboard: delete, select all, copy/cut/paste, nudge
        if !ui.ctx().egui_wants_keyboard_input() && active < s.seq.tracks.len() {
            let (events, del, all, esc, l, r, u, dn, shift) = ui.input(|i| {
                (
                    i.events.clone(),
                    i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace),
                    i.modifiers.command && i.key_pressed(Key::A),
                    i.key_pressed(Key::Escape),
                    i.key_pressed(Key::ArrowLeft),
                    i.key_pressed(Key::ArrowRight),
                    i.key_pressed(Key::ArrowUp),
                    i.key_pressed(Key::ArrowDown),
                    i.modifiers.shift,
                )
            });
            let mut remove = del;
            for e in &events {
                match e {
                    Event::Copy | Event::Cut if !self.selected.is_empty() => {
                        ui.ctx().copy_text(edit::to_text(&edit::copy(chan, &self.selected)));
                        remove |= matches!(e, Event::Cut);
                    }
                    Event::Paste(text) => {
                        if let Some(clip) = edit::from_text(text) {
                            let at = edit::snap_floor(playhead.unwrap_or(self.scroll_tick.max(0.0) as u32) as f32, self.grid);
                            self.selected.clear();
                            for spec in edit::pasted(&clip, at) {
                                let id = s.seq.add_note(active, spec);
                                self.selected.insert(id);
                            }
                            actions.push(Action::Edited);
                        }
                    }
                    _ => {}
                }
            }
            if remove && !self.selected.is_empty() {
                for id in std::mem::take(&mut self.selected) {
                    s.seq.set_note(active, id, None);
                }
                actions.push(Action::Edited);
            }
            if all {
                self.selected = chan.iter().map(|n| n.id).collect();
            }
            if esc {
                self.selected.clear();
            }
            let g = i64::from(self.grid.max(1));
            let oct = if shift { 12 } else { 1 };
            let nudge = match (l, r, u, dn) {
                (true, ..) => Some((-g, 0)),
                (_, true, ..) => Some((g, 0)),
                (_, _, true, _) => Some((0, oct)),
                (.., true) => Some((0, -oct)),
                _ => None,
            };
            if let Some((dt, dk)) = nudge
                && !self.selected.is_empty()
            {
                for (id, spec) in edit::moved(chan, &self.selected, dt, dk) {
                    s.seq.set_note(active, id, Some(spec));
                }
                actions.push(Action::Edited);
            }
        }

        // painting
        let painter = ui.painter_at(rect);
        painter.rect_filled(rect, 0.0, Color32::from_gray(22));
        // key rows
        let lo_key = key_of(area.bottom() - 0.5);
        let hi_key = key_of(area.top() + 0.5);
        let lit = |k: u8| v.lit.contains(&k) || self.held_key == Some(k) || self.click_off.is_some_and(|c| c.0 == k);
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
            let kc = if lit(k) {
                channel_color(active)
            } else if black {
                Color32::from_gray(40)
            } else {
                Color32::from_gray(200)
            };
            painter.rect_filled(kr, 0.0, kc);
            if !black && self.row_h >= 5.0 {
                painter.hline(kr.x_range(), kr.bottom(), Stroke::new(1.0, Color32::from_gray(120)));
            }
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
        let mut t = t0 / step * step;
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
        let painter_a = painter.with_clip_rect(area);
        let note_rect = |start: u32, len: u32, key: u8| {
            let (x0, x1) = (x_of(start as f32), x_of((start + len) as f32).max(x_of(start as f32) + 2.0));
            let y = y_of(f32::from(key));
            Rect::from_min_max(pos2(x0, y + 0.5), pos2(x1, y + self.row_h - 0.5))
        };
        let visible = |r: &Rect| r.right() >= area.left() && r.left() <= area.right() && r.bottom() >= area.top() && r.top() <= area.bottom();
        let fill = |n_vel: u8, c: Color32| {
            // brightness by velocity
            let f = 0.45 + 0.55 * f32::from(n_vel) / 127.0;
            Color32::from_rgb((f32::from(c.r()) * f) as u8, (f32::from(c.g()) * f) as u8, (f32::from(c.b()) * f) as u8)
        };
        if self.show_all {
            for (ch, ns) in notes.iter().enumerate() {
                if ch != active {
                    for n in ns {
                        let r = note_rect(n.start, n.len, n.key);
                        if visible(&r) {
                            painter_a.rect_filled(r, 1.0, channel_color(ch).gamma_multiply(0.3));
                        }
                    }
                }
            }
        }
        // the notes as the drag would leave them
        let moving: Vec<(u32, NoteSpec)> = match self.drag {
            Some(Drag { kind: DragKind::Move { .. }, delta, .. }) if delta != (0, 0) => edit::moved(chan, &self.selected, delta.0, delta.1),
            Some(Drag { kind: DragKind::Resize, delta, .. }) if delta != (0, 0) => edit::resized(chan, &self.selected, delta.0),
            _ => Vec::new(),
        };
        let boxed: Selection = match self.drag {
            Some(Drag { kind: DragKind::Box { tick, key, .. }, corner, .. }) => edit::in_box(chan, tick.max(0.0) as u32, corner.0.max(0.0) as u32, key, corner.1),
            _ => Selection::new(),
        };
        let c = channel_color(active);
        for n in chan {
            if moving.iter().any(|m| m.0 == n.id) {
                continue;
            }
            let r = note_rect(n.start, n.len, n.key);
            if !visible(&r) {
                continue;
            }
            painter_a.rect_filled(r, 1.5, fill(n.vel, c));
            let sel = self.selected.contains(&n.id) || boxed.contains(&n.id);
            if sel {
                painter_a.rect_stroke(r.expand(0.5), 1.5, Stroke::new(1.5, Color32::WHITE), StrokeKind::Outside);
            } else {
                painter_a.rect_stroke(r, 1.5, Stroke::new(1.0, Color32::from_black_alpha(160)), StrokeKind::Inside);
            }
        }
        for (_, sp) in &moving {
            let r = note_rect(sp.start, sp.len, sp.key);
            painter_a.rect_filled(r, 1.5, fill(sp.vel, c));
            painter_a.rect_stroke(r.expand(0.5), 1.5, Stroke::new(1.5, Color32::WHITE), StrokeKind::Outside);
        }
        if let Some(d) = self.drag
            && let Some(n) = d.note
            && let Some((_, sp)) = moving.iter().find(|m| m.0 == n.id)
        {
            let r = note_rect(sp.start, sp.len, sp.key);
            let more = if moving.len() > 1 { format!("  ({} notes)", moving.len()) } else { String::new() };
            painter.text(r.right_top() + vec2(4.0, -2.0), Align2::LEFT_BOTTOM, format!("{} {} len {}{more}", key_name(sp.key), bar_beat(sp.start), len_name(sp.len)), FontId::proportional(11.0), Color32::WHITE);
        }
        // notes being recorded
        for g in v.ghosts {
            let r = note_rect(g.start, g.len.max(1), g.key);
            if visible(&r) {
                painter_a.rect_filled(r, 1.5, Color32::from_rgba_unmultiplied(255, 90, 90, 120));
                painter_a.rect_stroke(r, 1.5, Stroke::new(1.0, Color32::from_rgb(255, 120, 120)), StrokeKind::Inside);
            }
        }
        // the selection box
        if let Some(Drag { kind: DragKind::Box { tick, key, .. }, corner, .. }) = self.drag {
            let r = Rect::from_two_pos(pos2(x_of(tick), y_of(f32::from(key)) + self.row_h), pos2(x_of(corner.0), y_of(f32::from(corner.1))));
            painter_a.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(120, 170, 255, 30));
            painter_a.rect_stroke(r, 0.0, Stroke::new(1.0, Color32::from_rgb(140, 180, 255)), StrokeKind::Inside);
        }
        // velocity lane
        painter.rect_filled(vel, 0.0, Color32::from_gray(16));
        painter.text(pos2(rect.left() + 4.0, vel.center().y), Align2::LEFT_CENTER, "vel", FontId::proportional(11.0), Color32::from_gray(140));
        for n in chan {
            let x = x_of(n.start as f32);
            if x < vel.left() - 4.0 || x > vel.right() {
                continue;
            }
            let value = match self.drag {
                Some(Drag { kind: DragKind::Velocity, note: Some(m), vel: dv, .. }) if m.id == n.id => dv,
                _ => n.vel,
            };
            let h = f32::from(value) / 127.0 * (vel.height() - 4.0);
            let selected = self.selected.contains(&n.id);
            painter.vline(x, (vel.bottom() - h)..=vel.bottom(), Stroke::new(if selected { 3.0 } else { 2.0 }, if selected { Color32::WHITE } else { c }));
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
        draw_markers(&painter, ruler, vel.bottom(), x_of(da as f32), x_of(db as f32));
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
        // what is under the pointer
        if self.drag.is_none()
            && let Some(p) = pointer
        {
            let tip = if let Some((n, _)) = hit(p) {
                Some(format!("{}  vel {}\nbar {}  tick {}\nlength {}", key_name(n.key), n.vel, bar_beat(n.start), n.start, len_name(n.len)))
            } else if keys.contains(p) {
                Some(format!("{}: click or hold to hear it", key_name(key_of(p.y))))
            } else {
                vel_hit(p).map(|n| format!("{} vel {}", key_name(n.key), n.vel))
            };
            if let Some(t) = tip {
                tooltip(&painter, rect, p, &t);
            }
        }
        actions
    }

    fn snap(&self, t: f32) -> u32 {
        edit::snap(t, self.grid)
    }

    fn snap_floor(&self, t: f32) -> u32 {
        edit::snap_floor(t, self.grid)
    }
}

/// The loop markers A (green) and B (red) in a ruler, with their lines
/// down to `bottom`.
pub fn draw_markers(painter: &egui::Painter, ruler: Rect, bottom: f32, xa: f32, xb: f32) {
    for (x, c, label) in [(xa, Color32::from_rgb(90, 220, 120), "A"), (xb, Color32::from_rgb(240, 110, 90), "B")] {
        if x >= ruler.left() - 8.0 && x <= ruler.right() + 8.0 {
            painter.vline(x, ruler.top()..=bottom, Stroke::new(1.5, c.gamma_multiply(0.8)));
            let tri = vec![pos2(x - 6.0, ruler.bottom() - 11.0), pos2(x + 6.0, ruler.bottom() - 11.0), pos2(x, ruler.bottom())];
            painter.add(egui::Shape::convex_polygon(tri, c, Stroke::NONE));
            painter.text(pos2(x, ruler.bottom() - 12.0), Align2::CENTER_BOTTOM, label, FontId::proportional(10.0), c);
        }
    }
}

/// A small label box next to the pointer, kept inside `bounds`.
pub fn tooltip(painter: &egui::Painter, bounds: Rect, p: Pos2, text: &str) {
    let font = FontId::proportional(12.0);
    let galley = painter.layout_no_wrap(text.to_string(), font, Color32::from_gray(235));
    let size = galley.size() + vec2(12.0, 8.0);
    let mut at = p + vec2(14.0, 16.0);
    if at.x + size.x > bounds.right() {
        at.x = p.x - size.x - 6.0;
    }
    if at.y + size.y > bounds.bottom() {
        at.y = p.y - size.y - 6.0;
    }
    let r = Rect::from_min_size(at, size);
    painter.rect_filled(r, 4.0, Color32::from_rgba_unmultiplied(20, 20, 24, 235));
    painter.rect_stroke(r, 4.0, Stroke::new(1.0, Color32::from_gray(90)), StrokeKind::Inside);
    painter.galley(at + vec2(6.0, 4.0), galley, Color32::from_gray(235));
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
        v.fit(&notes, 1000.0, false);
        assert_eq!(v.active, 2);
        assert_eq!(v.top_key, 73.0);
        v.px_per_tick = 3.0;
        v.fit(&notes, 1000.0, true);
        assert_eq!(v.px_per_tick, 3.0);
    }

    #[test]
    fn lengths_in_words() {
        assert_eq!(len_name(24), "1/8");
        assert_eq!(len_name(36), "3 x 1/16");
        assert_eq!(len_name(384), "2 x 1 bar");
        assert_eq!(len_name(29), "29 ticks");
    }
}
