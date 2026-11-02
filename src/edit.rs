//! Note selections and what is done with them: box select, moving and
//! nudging a group, copy and paste (through the clipboard as text, so it
//! also goes between two windows).

use std::collections::BTreeSet;

use crate::seq::{Note, NoteSpec};

/// Note ids of one channel.
pub type Selection = BTreeSet<u32>;

pub fn spec(n: &Note) -> NoteSpec {
    NoteSpec { start: n.start, len: n.len, key: n.key, vel: n.vel }
}

/// The notes in a box: overlapping ticks `t0..t1` with a key in `k0..=k1`.
pub fn in_box(notes: &[Note], t0: u32, t1: u32, k0: u8, k1: u8) -> Selection {
    let (t0, t1) = (t0.min(t1), t0.max(t1));
    let (k0, k1) = (k0.min(k1), k0.max(k1));
    notes.iter().filter(|n| n.key >= k0 && n.key <= k1 && n.start < t1.max(t0 + 1) && n.end().max(n.start + 1) > t0).map(|n| n.id).collect()
}

/// The selected notes moved by `dt` ticks and `dk` keys, as a group: the
/// move is cut short so that none goes before tick 0 or off the keyboard.
pub fn moved(notes: &[Note], sel: &Selection, dt: i64, dk: i32) -> Vec<(u32, NoteSpec)> {
    let picked: Vec<&Note> = notes.iter().filter(|n| sel.contains(&n.id)).collect();
    let Some(first) = picked.iter().map(|n| n.start).min() else { return Vec::new() };
    let lo = picked.iter().map(|n| i32::from(n.key)).min().unwrap_or(0);
    let hi = picked.iter().map(|n| i32::from(n.key)).max().unwrap_or(127);
    let dt = dt.max(-i64::from(first));
    let dk = dk.clamp(-lo, 127 - hi);
    picked
        .iter()
        .map(|n| (n.id, NoteSpec { start: (i64::from(n.start) + dt) as u32, key: (i32::from(n.key) + dk) as u8, ..spec(n) }))
        .collect()
}

/// The selected notes made `dl` ticks longer (at least one tick each).
pub fn resized(notes: &[Note], sel: &Selection, dl: i64) -> Vec<(u32, NoteSpec)> {
    notes.iter().filter(|n| sel.contains(&n.id)).map(|n| (n.id, NoteSpec { len: (i64::from(n.len) + dl).max(1) as u32, ..spec(n) })).collect()
}

/// The selected notes for the clipboard: starts from the first one.
pub fn copy(notes: &[Note], sel: &Selection) -> Vec<NoteSpec> {
    let picked: Vec<&Note> = notes.iter().filter(|n| sel.contains(&n.id)).collect();
    let first = picked.iter().map(|n| n.start).min().unwrap_or(0);
    let mut v: Vec<NoteSpec> = picked.iter().map(|n| NoteSpec { start: n.start - first, ..spec(n) }).collect();
    v.sort_by_key(|n| (n.start, n.key));
    v
}

const MAGIC: &str = "ffx-music-studio notes v1";

/// The clipboard text of copied notes: a header line, then one note a line
/// (start, length, key, velocity).
pub fn to_text(clip: &[NoteSpec]) -> String {
    let mut s = String::from(MAGIC);
    for n in clip {
        s.push_str(&format!("\n{} {} {} {}", n.start, n.len, n.key, n.vel));
    }
    s
}

pub fn from_text(s: &str) -> Option<Vec<NoteSpec>> {
    let mut lines = s.lines();
    if lines.next()?.trim() != MAGIC {
        return None;
    }
    let mut v = Vec::new();
    for l in lines.filter(|l| !l.trim().is_empty()) {
        let f: Vec<u32> = l.split_whitespace().map(str::parse).collect::<Result<_, _>>().ok()?;
        if f.len() != 4 {
            return None;
        }
        v.push(NoteSpec { start: f[0], len: f[1].max(1), key: f[2].min(127) as u8, vel: f[3].clamp(1, 127) as u8 });
    }
    Some(v)
}

/// Copied notes placed at tick `at`.
pub fn pasted(clip: &[NoteSpec], at: u32) -> Vec<NoteSpec> {
    clip.iter().map(|n| NoteSpec { start: n.start + at, ..*n }).collect()
}

/// `t` snapped down to the grid.
pub fn snap_floor(t: f32, grid: u32) -> u32 {
    let g = grid.max(1) as f32;
    ((t.max(0.0) / g).floor() * g) as u32
}

/// `t` snapped to the nearest grid line.
pub fn snap(t: f32, grid: u32) -> u32 {
    let g = grid.max(1) as f32;
    ((t.max(0.0) / g).round() * g) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seq::Off;

    fn n(id: u32, start: u32, len: u32, key: u8) -> Note {
        Note { id, start, len, key, vel: 100, on: 0, off: Off::Open }
    }

    fn notes() -> Vec<Note> {
        vec![n(0, 0, 48, 60), n(1, 48, 48, 64), n(2, 96, 24, 67), n(3, 200, 10, 127)]
    }

    #[test]
    fn box_selection() {
        let ns = notes();
        assert_eq!(in_box(&ns, 40, 100, 60, 70), [0, 1, 2].into());
        // dragged the other way round
        assert_eq!(in_box(&ns, 100, 40, 70, 60), [0, 1, 2].into());
        assert_eq!(in_box(&ns, 48, 49, 0, 127), [1].into());
        assert_eq!(in_box(&ns, 0, 1000, 65, 120), [2].into());
    }

    #[test]
    fn moving_a_group_stays_in_range() {
        let ns = notes();
        let sel: Selection = [0, 1].into();
        let m = moved(&ns, &sel, 24, 2);
        assert_eq!(m, vec![(0, NoteSpec { start: 24, len: 48, key: 62, vel: 100 }), (1, NoteSpec { start: 72, len: 48, key: 66, vel: 100 })]);
        // before tick 0: the group stops at 0, keeping its spacing
        let m = moved(&ns, &sel, -100, 0);
        assert_eq!((m[0].1.start, m[1].1.start), (0, 48));
        // off the top: note 3 is at 127 already
        let m = moved(&ns, &[2, 3].into(), 0, 5);
        assert_eq!((m[0].1.key, m[1].1.key), (67, 127));
        assert!(moved(&ns, &Selection::new(), 5, 5).is_empty());
        let r = resized(&ns, &[2].into(), -100);
        assert_eq!(r[0].1.len, 1);
    }

    #[test]
    fn copy_and_paste_through_text() {
        let ns = notes();
        let clip = copy(&ns, &[1, 2].into());
        assert_eq!(clip, vec![NoteSpec { start: 0, len: 48, key: 64, vel: 100 }, NoteSpec { start: 48, len: 24, key: 67, vel: 100 }]);
        let text = to_text(&clip);
        assert_eq!(from_text(&text), Some(clip.clone()));
        assert_eq!(from_text("hello"), None);
        assert_eq!(from_text(&format!("{MAGIC}\n1 2 x 4")), None);
        let p = pasted(&clip, 480);
        assert_eq!((p[0].start, p[1].start), (480, 528));
    }

    #[test]
    fn snapping() {
        assert_eq!(snap_floor(47.9, 12), 36);
        assert_eq!(snap(47.9, 12), 48);
        assert_eq!(snap(-5.0, 12), 0);
        assert_eq!(snap_floor(5.0, 0), 5);
    }
}
