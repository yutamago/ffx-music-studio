//! A `musicNNN.bgm` as an editable list of events per track, and back.
//!
//! Every track is a list of [`Event`]s in stream order, each at an absolute
//! tick (48 per quarter note). Encoding writes them back with the deltas
//! between their ticks; an unedited sequence encodes to the same bytes.
//!
//! Notes are derived from the events ([`Track::notes`]): a key-on
//! (`0x10..0x13`) starts one, the key-off of its key (`0x18..0x1b`; with
//! header flag `0x10000`, also the driver's automatic key-off one tick before
//! the next event) ends it. The driver keeps the current key and velocity
//! between commands (`0x10` reuses both), so before the first note edit a
//! track is *normalized* ([`Track::normalize`]): every key-on becomes
//! `0x11 key vel`, every key-off `0x1a key`, each note gets its own key-off
//! event, and automatic key-offs become explicit ones (with `0x2e` at the
//! start). The driver plays a normalized track exactly as the original
//! (tested against the disc in `player.rs`). Notes of a normalized track are
//! linked to their two events by an id, so they can be moved, resized and
//! deleted one by one.

use anyhow::{Context, Result};
use demaster::sqseq::{Bgm, arg_bytes, varlen};

/// Ticks per quarter note (the header says 48 for every FFX song).
pub const PPQN: u32 = 48;

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub tick: u32,
    pub op: u8,
    /// The argument bytes as stored (variable-length numbers encoded).
    pub args: Vec<u8>,
    /// Normalized tracks: the note this key-on/key-off belongs to.
    pub note: Option<u32>,
}

impl Event {
    fn new(tick: u32, op: u8, args: Vec<u8>) -> Self {
        Event { tick, op, args, note: None }
    }

    /// The driver stops reading the track after this command.
    pub fn ends_track(&self) -> bool {
        self.op == 0 || self.op & 0x80 != 0 || arg_bytes(self.op).is_none()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Track {
    /// Delta before the first event when there is none.
    pub lead: u32,
    pub events: Vec<Event>,
    /// Delta after the last event if it does not end the track (the stream
    /// simply runs out).
    pub final_delta: Option<u32>,
    /// Bytes after the end command (never read by the driver).
    pub tail: Vec<u8>,
    pub normalized: bool,
}

/// How a note ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Off {
    /// A key-off (or end) event.
    Event(usize),
    /// The driver's automatic key-off at this tick, before event `before`.
    Auto { tick: u32, before: usize },
    /// No key-off: a tie took the voice over, or the track ended.
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Note {
    /// Raw tracks: the index in [`Track::notes`]; normalized tracks: the id
    /// on its events (the same number right after normalizing).
    pub id: u32,
    pub start: u32,
    pub len: u32,
    pub key: u8,
    pub vel: u8,
    /// The key-on event.
    pub on: usize,
    pub off: Off,
}

impl Note {
    pub fn end(&self) -> u32 {
        self.start + self.len
    }
}

/// Header flag: notes end one tick before the next event.
const AUTO_OFF: u32 = 0x10000;

fn is_on(op: u8) -> bool {
    (0x10..=0x13).contains(&op)
}
fn is_off(op: u8) -> bool {
    (0x18..=0x1b).contains(&op)
}

fn push_varlen(out: &mut Vec<u8>, v: u32) {
    let mut groups = vec![(v & 0x7f) as u8];
    let mut v = v >> 7;
    while v != 0 {
        groups.push((v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    out.extend(groups.iter().rev());
}

impl Track {
    fn decode(d: &[u8]) -> Track {
        let mut p = 0;
        let lead = varlen(d, &mut p);
        let mut tick = lead;
        let mut events = Vec::new();
        let mut final_delta = None;
        while p < d.len() {
            let op = d[p];
            p += 1;
            let start = p;
            let ends = op & 0x80 != 0 || arg_bytes(op).is_none() || op == 0;
            if let Some(layout) = arg_bytes(op).filter(|_| op & 0x80 == 0) {
                for a in layout {
                    if *a == 0 {
                        varlen(d, &mut p);
                    } else {
                        p += 1;
                    }
                }
            }
            let p_args = p.min(d.len());
            events.push(Event::new(tick, op, d[start..p_args].to_vec()));
            p = p_args;
            if ends {
                break;
            }
            if p >= d.len() {
                break;
            }
            let delta = varlen(d, &mut p);
            if p >= d.len() {
                final_delta = Some(delta);
                break;
            }
            tick += delta;
        }
        Track { lead, events, final_delta, tail: d[p.min(d.len())..].to_vec(), normalized: false }
    }

    fn encode(&self, out: &mut Vec<u8>) {
        let first = self.events.first().map_or(self.lead, |e| e.tick);
        push_varlen(out, first);
        for (i, e) in self.events.iter().enumerate() {
            out.push(e.op);
            out.extend_from_slice(&e.args);
            if e.ends_track() {
                break;
            }
            match self.events.get(i + 1) {
                Some(n) => push_varlen(out, n.tick - e.tick),
                None => {
                    if let Some(d) = self.final_delta {
                        push_varlen(out, d);
                    }
                }
            }
        }
        out.extend_from_slice(&self.tail);
    }

    /// Tick of the command that ends the track (or of its last event).
    pub fn end_tick(&self) -> u32 {
        self.events.last().map_or(self.lead, |e| e.tick)
    }

    /// The notes, in key-on order.
    pub fn notes(&self, header_flags: u32) -> Vec<Note> {
        if self.normalized {
            return self.linked_notes(header_flags);
        }
        self.analyse(header_flags).0
    }

    /// Notes of a raw track as the driver plays them, and the automatic
    /// key-offs (tick, key, before which event).
    fn analyse(&self, header_flags: u32) -> (Vec<Note>, Vec<(u32, u8, usize)>) {
        let mut notes: Vec<Note> = Vec::new();
        let mut autos = Vec::new();
        let mut sounding: Vec<usize> = Vec::new();
        let (mut key, mut vel) = (0u8, 0u8);
        let mut auto = header_flags & AUTO_OFF != 0;
        let mut tie = false;
        let release = |notes: &mut Vec<Note>, sounding: &mut Vec<usize>, key: u8, at: u32, off: Off| {
            sounding.retain(|&n| {
                if notes[n].key == key {
                    notes[n].len = at - notes[n].start;
                    notes[n].off = off;
                    false
                } else {
                    true
                }
            });
        };
        for (i, e) in self.events.iter().enumerate() {
            if i > 0 {
                let prev = self.events[i - 1].tick;
                if auto && e.tick >= prev + 2 {
                    let at = e.tick - 1;
                    autos.push((at, key, i));
                    if !tie {
                        release(&mut notes, &mut sounding, key, at, Off::Auto { tick: at, before: i });
                    }
                }
            }
            let a = |k: usize| e.args.get(k).copied().unwrap_or(0);
            match e.op {
                op if is_on(op) => {
                    match op {
                        0x11 => (key, vel) = (a(0), a(1)),
                        0x12 => key = a(0),
                        0x13 => vel = a(0),
                        _ => {}
                    }
                    if tie {
                        // the voice goes on with the new key (legato)
                        for &n in &sounding {
                            notes[n].len = e.tick - notes[n].start;
                        }
                        sounding.clear();
                    }
                    sounding.push(notes.len());
                    notes.push(Note {
                        id: notes.len() as u32,
                        start: e.tick,
                        len: 0,
                        key,
                        vel,
                        on: i,
                        off: Off::Open,
                    });
                }
                op if is_off(op) => {
                    if op == 0x19 || op == 0x1a {
                        key = a(0);
                    }
                    if !tie {
                        release(&mut notes, &mut sounding, key, e.tick, Off::Event(i));
                    }
                }
                0x2a => tie = true,
                0x2b => tie = false,
                0x2e => auto = false,
                0x2f => auto = true,
                _ if e.ends_track() => {
                    tie = false;
                    release(&mut notes, &mut sounding, key, e.tick, Off::Event(i));
                }
                _ => {}
            }
        }
        let end = self.end_tick();
        for n in sounding {
            notes[n].len = end - notes[n].start;
        }
        (notes, autos)
    }

    fn linked_notes(&self, header_flags: u32) -> Vec<Note> {
        let mut notes = Vec::new();
        // notes without a key-off (a tie took the voice over, or the end):
        // as long as the driver plays them
        let mut analysed: Option<Vec<Note>> = None;
        for (i, e) in self.events.iter().enumerate() {
            let Some(id) = e.note.filter(|_| e.op == 0x11) else { continue };
            let off = self.events.iter().position(|x| x.note == Some(id) && x.op == 0x1a);
            let len = match off {
                Some(o) => self.events[o].tick - e.tick,
                None => {
                    let a = analysed.get_or_insert_with(|| self.analyse(header_flags).0);
                    a.iter().find(|n| n.on == i).map_or(0, |n| n.len)
                }
            };
            notes.push(Note {
                id,
                start: e.tick,
                len,
                key: e.args[0],
                vel: e.args[1],
                on: i,
                off: off.map_or(Off::Open, Off::Event),
            });
        }
        notes
    }

    /// Rewrites the track so every note has its own `0x11 key vel` and
    /// `0x1a key` (see the module docs). The driver plays the result as the
    /// original; note ids are the indices of [`Track::notes`] before.
    pub fn normalize(&mut self, header_flags: u32) {
        if self.normalized {
            return;
        }
        let (notes, autos) = self.analyse(header_flags);
        let has_auto = header_flags & AUTO_OFF != 0 || self.events.iter().any(|e| e.op == 0x2f);
        // key-off events by the event (or automatic key-off) that ends them
        let mut ends_at_event: Vec<Vec<u32>> = vec![Vec::new(); self.events.len()];
        let mut ends_auto: Vec<Vec<u32>> = vec![Vec::new(); self.events.len() + 1];
        let mut on_note: Vec<Option<u32>> = vec![None; self.events.len()];
        for n in &notes {
            on_note[n.on] = Some(n.id);
            match n.off {
                Off::Event(i) => ends_at_event[i].push(n.id),
                Off::Auto { before, .. } => ends_auto[before].push(n.id),
                Off::Open => {}
            }
        }
        let mut key = 0u8;
        let mut vel = 0u8;
        let mut out = Vec::with_capacity(self.events.len() + notes.len());
        let mut autos = autos.into_iter().peekable();
        for (i, e) in self.events.iter().enumerate() {
            if i == 0 && has_auto {
                out.push(Event::new(e.tick, 0x2e, vec![]));
            }
            while let Some(&(at, k, before)) = autos.peek() {
                if before != i {
                    break;
                }
                autos.next();
                let ids = std::mem::take(&mut ends_auto[i]);
                if ids.is_empty() {
                    out.push(Event::new(at, 0x1a, vec![k]));
                }
                for id in ids {
                    out.push(Event { note: Some(id), ..Event::new(at, 0x1a, vec![k]) });
                }
            }
            let a = |k: usize| e.args.get(k).copied().unwrap_or(0);
            match e.op {
                op if is_on(op) => {
                    match op {
                        0x11 => (key, vel) = (a(0), a(1)),
                        0x12 => key = a(0),
                        0x13 => vel = a(0),
                        _ => {}
                    }
                    out.push(Event { note: on_note[i], ..Event::new(e.tick, 0x11, vec![key, vel]) });
                }
                op if is_off(op) => {
                    if op == 0x19 || op == 0x1a {
                        key = a(0);
                    }
                    let ids = &ends_at_event[i];
                    if ids.is_empty() {
                        out.push(Event::new(e.tick, 0x1a, vec![key]));
                    }
                    for &id in ids {
                        out.push(Event { note: Some(id), ..Event::new(e.tick, 0x1a, vec![key]) });
                    }
                }
                0x2e | 0x2f if has_auto => {}
                _ => {
                    if e.ends_track() {
                        for &id in &ends_at_event[i] {
                            out.push(Event { note: Some(id), ..Event::new(e.tick, 0x1a, vec![key]) });
                        }
                    }
                    out.push(e.clone());
                }
            }
        }
        self.events = out;
        self.normalized = true;
    }

    fn next_id(&self) -> u32 {
        self.events.iter().filter_map(|e| e.note).max().map_or(0, |m| m + 1)
    }

    /// Where a key-on at `tick` goes: after the events of that tick, before
    /// the end of the track.
    fn on_slot(&self, tick: u32) -> usize {
        self.events.iter().position(|e| e.tick > tick || (e.tick == tick && e.ends_track())).unwrap_or(self.events.len())
    }

    /// Where a key-off at `tick` goes: before the events of that tick.
    fn off_slot(&self, tick: u32) -> usize {
        self.events.iter().position(|e| e.tick >= tick).unwrap_or(self.events.len())
    }

    /// Latest tick a note may end at: the track's end command.
    pub fn note_limit(&self) -> u32 {
        match self.events.last() {
            Some(e) if e.ends_track() => e.tick,
            Some(e) => e.tick + self.final_delta.unwrap_or(0),
            None => self.lead,
        }
    }

    fn remove_note(&mut self, id: u32) {
        self.events.retain(|e| e.note != Some(id));
    }

    fn insert_note(&mut self, id: u32, n: &NoteSpec) {
        let limit = self.note_limit();
        let start = n.start.min(limit.saturating_sub(1));
        let end = (start + n.len.max(1)).min(limit.max(start + 1));
        let key = n.key.min(127);
        let vel = n.vel.min(127);
        let on = self.on_slot(start);
        self.events.insert(on, Event { note: Some(id), ..Event::new(start, 0x11, vec![key, vel]) });
        let off = self.off_slot(end).max(on + 1);
        self.events.insert(off, Event { note: Some(id), ..Event::new(end, 0x1a, vec![key]) });
    }
}

/// A note as the editor sets it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NoteSpec {
    pub start: u32,
    pub len: u32,
    pub key: u8,
    pub vel: u8,
}

/// A whole sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct Seq {
    pub header: Vec<u8>,
    pub tracks: Vec<Track>,
    /// Bytes after the last track (padding, sometimes not zero), kept.
    pub trailer: Vec<u8>,
    /// Where the last track ended on the disc (the header's size field is
    /// that or a little more; it moves with the tracks' length).
    pub orig_end: usize,
}

impl Seq {
    pub fn decode(b: &[u8]) -> Result<Seq> {
        let bgm = Bgm::parse(b)?;
        let mut tracks = Vec::new();
        let mut end = 0x20;
        for &start in &bgm.tracks {
            let len = u32::from_le_bytes(b[start - 4..start].try_into()?) as usize;
            let d = b.get(start..start + len).context("track out of range")?;
            tracks.push(Track::decode(d));
            end = start + len;
        }
        Ok(Seq { header: b[..0x20].to_vec(), tracks, trailer: b[end..].to_vec(), orig_end: end })
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = self.header.clone();
        for t in &self.tracks {
            let mut body = Vec::new();
            t.encode(&mut body);
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&body);
        }
        let size = i64::from(u32::from_le_bytes(self.header[0x10..0x14].try_into().expect("header")));
        let size = (size + out.len() as i64 - self.orig_end as i64).max(0) as u32;
        out[0x10..0x14].copy_from_slice(&size.to_le_bytes());
        out.extend_from_slice(&self.trailer);
        out
    }

    pub fn to_bgm(&self) -> Result<Bgm> {
        Bgm::parse(&self.encode())
    }

    pub fn flags(&self) -> u32 {
        u32::from_le_bytes(self.header[0x14..0x18].try_into().expect("header"))
    }

    /// The song's last tick (where every track has ended).
    pub fn end_tick(&self) -> u32 {
        self.tracks.iter().map(Track::end_tick).max().unwrap_or(0)
    }

    /// The loop point (`0x02` in track 1), as the driver honours it.
    pub fn loop_start(&self) -> Option<u32> {
        self.tracks.get(1)?.events.iter().find(|e| e.op == 0x02).map(|e| e.tick)
    }

    /// Where the song jumps back: the first `0x03`, else the end.
    pub fn loop_end(&self) -> u32 {
        self.tracks
            .iter()
            .flat_map(|t| t.events.iter().filter(|e| e.op == 0x03).map(|e| e.tick))
            .min()
            .unwrap_or_else(|| self.end_tick())
    }

    pub fn notes(&self, track: usize) -> Vec<Note> {
        self.tracks[track].notes(self.flags())
    }

    /// Changes a note (by its id from [`Seq::notes`]); `None` deletes it.
    pub fn set_note(&mut self, track: usize, id: u32, spec: Option<NoteSpec>) {
        let flags = self.flags();
        let t = &mut self.tracks[track];
        t.normalize(flags);
        t.remove_note(id);
        if let Some(n) = spec {
            t.insert_note(id, &n);
        }
    }

    /// Adds a note; returns its id.
    pub fn add_note(&mut self, track: usize, spec: NoteSpec) -> u32 {
        let flags = self.flags();
        let t = &mut self.tracks[track];
        t.normalize(flags);
        let id = t.next_id();
        t.insert_note(id, &spec);
        id
    }

    /// Tempo commands (`0x08`): (track, event, tick, BPM).
    pub fn tempos(&self) -> Vec<(usize, usize, u32, u8)> {
        let mut v = Vec::new();
        for (ti, t) in self.tracks.iter().enumerate() {
            for (i, e) in t.events.iter().enumerate() {
                if e.op == 0x08 {
                    v.push((ti, i, e.tick, e.args[0]));
                }
            }
        }
        v.sort_by_key(|x| (x.2, x.0));
        v
    }

    pub fn set_tempo(&mut self, track: usize, event: usize, bpm: u8) {
        let e = &mut self.tracks[track].events[event];
        if e.op == 0x08 {
            e.args[0] = bpm.max(1);
        }
    }

    /// Seconds from the start to `tick` with the tempo commands (the song
    /// plays at 1 BPM before its first one; one tick is 1 / (0.8 BPM) s).
    pub fn seconds_at(&self, tick: u32) -> f64 {
        let tempos = self.tempos();
        let mut bpm = 1.0f64;
        let mut at = 0u32;
        let mut s = 0.0;
        for &(_, _, t, b) in &tempos {
            if t >= tick {
                break;
            }
            s += f64::from(t - at) / (0.8 * bpm);
            at = t;
            bpm = f64::from(b.max(1));
        }
        s + f64::from(tick - at) / (0.8 * bpm)
    }

    /// Is `track` ever heard (has a key-on)?
    pub fn has_notes(&self, track: usize) -> bool {
        self.tracks[track].events.iter().any(|e| is_on(e.op))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A BGM from tracks of (lead, [(op, args, delta)]).
    pub fn bgm(flags: u32, tracks: &[(u32, Vec<(u8, Vec<u8>, u32)>)]) -> Vec<u8> {
        let mut b = vec![0u8; 0x20];
        b[..4].copy_from_slice(b"BGM ");
        b[8] = tracks.len() as u8;
        b[9] = 0xff;
        b[12] = 0x7f;
        b[0xe..0x10].copy_from_slice(&48u16.to_le_bytes());
        b[0x14..0x18].copy_from_slice(&flags.to_le_bytes());
        for (lead, evs) in tracks {
            let mut body = Vec::new();
            push_varlen(&mut body, *lead);
            for (op, args, d) in evs {
                body.push(*op);
                body.extend_from_slice(args);
                if *op != 0 {
                    push_varlen(&mut body, *d);
                }
            }
            b.extend_from_slice(&(body.len() as u32).to_le_bytes());
            b.extend_from_slice(&body);
        }
        let n = b.len() as u32;
        b[0x10..0x14].copy_from_slice(&n.to_le_bytes());
        b.resize(b.len().next_multiple_of(8), 0);
        b
    }

    fn sample() -> Vec<u8> {
        bgm(
            0,
            &[
                (0, vec![(0x08, vec![120], 0), (0x00, vec![], 0)]),
                (
                    0,
                    vec![
                        (0x20, vec![0], 0),
                        (0x11, vec![60, 100], 48),
                        (0x18, vec![], 0),
                        (0x12, vec![62], 0),
                        (0x10, vec![], 24),
                        (0x02, vec![], 24),
                        (0x1a, vec![62], 200),
                        (0x13, vec![90], 300),
                        (0x00, vec![], 0),
                    ],
                ),
            ],
        )
    }

    #[test]
    fn decode_encode_is_identical() {
        let b = sample();
        let s = Seq::decode(&b).unwrap();
        assert_eq!(s.encode(), b);
        assert_eq!(s.tracks[1].events.len(), 9);
        assert_eq!(s.loop_start(), Some(72));
        assert_eq!(s.end_tick(), 596);
    }

    #[test]
    fn varlen_round_trip() {
        for v in [0, 1, 127, 128, 300, 16383, 16384, 1 << 21] {
            let mut b = Vec::new();
            push_varlen(&mut b, v);
            let mut p = 0;
            assert_eq!(varlen(&b, &mut p), v);
            assert_eq!(p, b.len());
        }
    }

    #[test]
    fn notes_of_a_raw_track() {
        let s = Seq::decode(&sample()).unwrap();
        let n = s.notes(1);
        // 60 (48 ticks), two 62s on the same key: one key-off ends both
        assert_eq!(n.len(), 4);
        assert_eq!((n[0].key, n[0].vel, n[0].start, n[0].len), (60, 100, 0, 48));
        assert_eq!((n[1].key, n[1].vel, n[1].start, n[1].len), (62, 100, 48, 48));
        assert_eq!((n[2].key, n[2].start, n[2].len), (62, 48, 48));
        assert_eq!((n[3].key, n[3].vel, n[3].start, n[3].len), (62, 90, 296, 300));
    }

    #[test]
    fn normalizing_keeps_the_notes() {
        let mut s = Seq::decode(&sample()).unwrap();
        let before = s.notes(1);
        let flags = s.flags();
        s.tracks[1].normalize(flags);
        let after = s.notes(1);
        assert_eq!(before.len(), after.len());
        for (a, b) in before.iter().zip(&after) {
            assert_eq!((a.id, a.start, a.len, a.key, a.vel), (b.id, b.start, b.len, b.key, b.vel));
        }
        // every key-on explicit, one key-off per note (+ the end's)
        assert!(s.tracks[1].events.iter().all(|e| !is_on(e.op) || e.op == 0x11));
        assert_eq!(s.tracks[1].events.iter().filter(|e| e.op == 0x1a).count(), 4);
        // still decodes and encodes
        let again = Seq::decode(&s.encode()).unwrap();
        assert_eq!(again.encode(), s.encode());
    }

    #[test]
    fn auto_key_off_becomes_explicit() {
        let b = bgm(
            AUTO_OFF,
            &[
                (0, vec![(0x00, vec![], 0)]),
                (0, vec![(0x20, vec![0], 0), (0x11, vec![60, 100], 10), (0x12, vec![64], 10), (0x00, vec![], 0)]),
            ],
        );
        let mut s = Seq::decode(&b).unwrap();
        let n = s.notes(1);
        assert_eq!((n[0].start, n[0].len), (0, 9));
        assert_eq!((n[1].start, n[1].len), (10, 9));
        s.tracks[1].normalize(AUTO_OFF);
        let ops: Vec<(u32, u8)> = s.tracks[1].events.iter().map(|e| (e.tick, e.op)).collect();
        assert_eq!(ops, vec![(0, 0x2e), (0, 0x20), (0, 0x11), (9, 0x1a), (10, 0x11), (19, 0x1a), (20, 0x00)]);
    }

    #[test]
    fn edit_notes() {
        let mut s = Seq::decode(&sample()).unwrap();
        // move the first note later and up
        s.set_note(1, 0, Some(NoteSpec { start: 100, len: 30, key: 67, vel: 50 }));
        let n = s.notes(1);
        let moved = n.iter().find(|x| x.id == 0).unwrap();
        assert_eq!((moved.start, moved.len, moved.key, moved.vel), (100, 30, 67, 50));
        // delete one, add one
        s.set_note(1, 1, None);
        let id = s.add_note(1, NoteSpec { start: 10, len: 5, key: 40, vel: 127 });
        let n = s.notes(1);
        assert_eq!(n.len(), 4);
        assert!(n.iter().all(|x| x.id != 1));
        let added = n.iter().find(|x| x.id == id).unwrap();
        assert_eq!((added.start, added.len, added.key), (10, 5, 40));
        // notes stay before the track's end
        s.set_note(1, id, Some(NoteSpec { start: 5000, len: 50, key: 40, vel: 1 }));
        let n = s.notes(1);
        let last = n.iter().find(|x| x.id == id).unwrap();
        assert!(last.end() <= s.tracks[1].note_limit());
        let again = Seq::decode(&s.encode()).unwrap();
        assert_eq!(again.tracks[1].events.len(), s.tracks[1].events.len());
    }

    #[test]
    fn tempo_and_time() {
        let mut s = Seq::decode(&sample()).unwrap();
        // 120 BPM: 96 ticks a second
        assert!((s.seconds_at(96) - 1.0).abs() < 1e-9);
        let (t, e, _, bpm) = s.tempos()[0];
        assert_eq!(bpm, 120);
        s.set_tempo(t, e, 60);
        assert!((s.seconds_at(96) - 2.0).abs() < 1e-9);
    }
}
