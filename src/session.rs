//! One loaded track with its edits (in memory only): the sequence, the
//! mixer, the imported instruments, the chip and loop settings, and the
//! undo history of all of them. The UI and `--render` both edit through
//! it; [`Session::settings`] and [`Session::bgm`] are what the player gets.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use demaster::sqseq::{Bgm, Mods, TrackMod, Wd};

use crate::player::{Chip, Settings};
use crate::seq::Seq;

#[derive(Debug, Clone, PartialEq)]
pub struct Channel {
    pub mute: bool,
    pub solo: bool,
    pub gain: f32,
    pub pan: i32,
    pub transpose: i32,
    /// Instrument instead of the track's own: (bank, instrument in the bank,
    /// its number in the session's bank).
    pub instrument: Option<(u16, usize, usize)>,
}

impl Default for Channel {
    fn default() -> Self {
        Channel { mute: false, solo: false, gain: 1.0, pan: 0, transpose: 0, instrument: None }
    }
}

/// Everything an edit changes, for undo and redo.
#[derive(Clone)]
struct State {
    seq: Seq,
    wd: Arc<Wd>,
    imports: BTreeMap<u16, u32>,
    channels: Vec<Channel>,
    chip: Chip,
    tempo: f64,
    speed: f64,
    master: f32,
    looping: bool,
    loop_range: Option<(u32, u32)>,
    version: u64,
}

impl State {
    /// The same edits (the sequence by its version: it only changes with
    /// one).
    fn same(&self, o: &State) -> bool {
        self.version == o.version
            && Arc::ptr_eq(&self.wd, &o.wd)
            && self.channels == o.channels
            && self.chip == o.chip
            && self.tempo == o.tempo
            && self.speed == o.speed
            && self.master == o.master
            && self.looping == o.looping
            && self.loop_range == o.loop_range
    }
}

/// Undo steps kept.
const HISTORY: usize = 300;

pub struct Session {
    pub id: u16,
    pub label: String,
    pub orig: Seq,
    pub seq: Seq,
    pub wd_orig: Arc<Wd>,
    /// The track's bank with imported instruments appended.
    pub wd: Arc<Wd>,
    /// Where each imported bank's samples went in `wd`.
    imports: BTreeMap<u16, u32>,
    pub channels: Vec<Channel>,
    pub chip: Chip,
    pub tempo: f64,
    pub speed: f64,
    /// The track's master volume (before the limiter; part of the track's
    /// edits, 1.0 when loaded or reset). The window's output volume is
    /// separate (`App::prefs`).
    pub master: f32,
    /// Loop at the end (else stop).
    pub looping: bool,
    /// The studio's loop range (ticks), `None`: the song's own loop.
    pub loop_range: Option<(u32, u32)>,
    /// Counts sequence changes (the player reloads when it moves).
    pub version: u64,
    /// The state as of the last [`Session::checkpoint`], the steps before
    /// it and the steps undone.
    committed: Option<State>,
    undo: Vec<State>,
    redo: Vec<State>,
}

impl Session {
    pub fn new(id: u16, label: String, bytes: &[u8], wd: Arc<Wd>) -> Result<Session> {
        let seq = Seq::decode(bytes)?;
        let n = seq.tracks.len();
        let mut s = Session {
            id,
            label,
            orig: seq.clone(),
            seq,
            wd_orig: wd.clone(),
            wd,
            imports: BTreeMap::new(),
            channels: vec![Channel::default(); n],
            chip: Chip::default(),
            tempo: 1.0,
            speed: 1.0,
            master: 1.0,
            looping: true,
            loop_range: None,
            version: 0,
            committed: None,
            undo: Vec::new(),
            redo: Vec::new(),
        };
        s.checkpoint();
        Ok(s)
    }

    /// Back to the track as on the disc (one undo step).
    pub fn reset(&mut self) {
        self.checkpoint();
        self.seq = self.orig.clone();
        self.wd = self.wd_orig.clone();
        self.imports.clear();
        self.channels = vec![Channel::default(); self.seq.tracks.len()];
        self.chip = Chip::default();
        self.tempo = 1.0;
        self.speed = 1.0;
        self.master = 1.0;
        self.looping = true;
        self.loop_range = None;
        self.version += 1;
        self.checkpoint();
    }

    fn state(&self) -> State {
        State {
            seq: self.seq.clone(),
            wd: self.wd.clone(),
            imports: self.imports.clone(),
            channels: self.channels.clone(),
            chip: self.chip.clone(),
            tempo: self.tempo,
            speed: self.speed,
            master: self.master,
            looping: self.looping,
            loop_range: self.loop_range,
            version: self.version,
        }
    }

    fn restore(&mut self, st: State) {
        let v = self.version;
        self.seq = st.seq;
        self.wd = st.wd;
        self.imports = st.imports;
        self.channels = st.channels;
        self.chip = st.chip;
        self.tempo = st.tempo;
        self.speed = st.speed;
        self.master = st.master;
        self.looping = st.looping;
        self.loop_range = st.loop_range;
        // a new version: the player reloads the sequence
        self.version = v + 1;
        self.committed = Some(self.state());
    }

    /// Records the edits since the last call as one undo step. The window
    /// calls it when the edits have settled (each frame without a mouse
    /// button down: a slider's drag is one step).
    pub fn checkpoint(&mut self) {
        let changed = match &self.committed {
            None => {
                self.committed = Some(self.state());
                return;
            }
            Some(c) => c.version != self.version || !self.state().same(c),
        };
        if changed {
            let prev = self.committed.replace(self.state()).expect("committed");
            self.undo.push(prev);
            if self.undo.len() > HISTORY {
                self.undo.remove(0);
            }
            self.redo.clear();
        }
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// One step back (edits not checkpointed yet count as a step).
    pub fn undo(&mut self) -> bool {
        self.checkpoint();
        let Some(prev) = self.undo.pop() else { return false };
        self.redo.push(self.state());
        self.restore(prev);
        true
    }

    pub fn redo(&mut self) -> bool {
        self.checkpoint();
        let Some(next) = self.redo.pop() else { return false };
        self.undo.push(self.state());
        self.restore(next);
        true
    }

    pub fn seq_changed(&mut self) {
        self.version += 1;
    }

    pub fn edited(&self) -> bool {
        self.seq != self.orig || self.settings() != Settings::default() || !Arc::ptr_eq(&self.wd, &self.wd_orig)
    }

    pub fn bgm(&self) -> Result<Arc<Bgm>> {
        Ok(Arc::new(self.seq.to_bgm()?))
    }

    /// The song's own loop: (loop point, where it jumps back).
    pub fn song_loop(&self) -> (Option<u32>, u32) {
        (self.seq.loop_start(), self.seq.loop_end())
    }

    /// The loop markers as shown: the studio's range or the song's loop.
    pub fn markers(&self) -> (u32, u32) {
        match self.loop_range {
            Some(r) => r,
            None => {
                let (a, b) = self.song_loop();
                (a.unwrap_or(0), b)
            }
        }
    }

    /// Sets the loop range (`None`: back to the song's loop).
    pub fn set_loop(&mut self, r: Option<(u32, u32)>) {
        self.loop_range = r.map(|(a, b)| {
            let end = self.seq.end_tick().max(1);
            let a = a.min(end - 1);
            (a, b.clamp(a + 1, end + 1))
        });
    }

    /// The mixer settings of channel `c` as the driver takes them (mute and
    /// solo as volume 0).
    pub fn track_mod(&self, c: usize) -> TrackMod {
        let solo = self.channels.iter().any(|c| c.solo);
        let ch = &self.channels[c];
        TrackMod {
            gain: if ch.mute || (solo && !ch.solo) { 0.0 } else { ch.gain },
            pan: ch.pan,
            transpose: ch.transpose,
            program: ch.instrument.map(|i| i.2),
        }
    }

    /// The instrument channel `c` plays: the one given to it, else its
    /// program at `tick` (the last `0x20` before it, or its first).
    pub fn program_at(&self, c: usize, tick: u32) -> Option<usize> {
        if let Some(i) = self.channels.get(c)?.instrument {
            return Some(i.2);
        }
        let ev = &self.seq.tracks.get(c)?.events;
        let progs = || ev.iter().filter(|e| e.op == 0x20).map(|e| (e.tick, usize::from(e.args[0])));
        progs().filter(|p| p.0 <= tick).last().or_else(|| progs().next()).map(|p| p.1)
    }

    pub fn settings(&self) -> Settings {
        let tracks = (0..self.channels.len()).map(|c| self.track_mod(c)).collect::<Vec<_>>();
        let tracks = if tracks.iter().all(|t| *t == TrackMod::default()) { Vec::new() } else { tracks };
        Settings {
            mods: Mods {
                tracks,
                tempo: if self.tempo == 1.0 { 0.0 } else { self.tempo },
                loop_range: self.loop_range,
                no_loop: !self.looping,
            },
            chip: self.chip.clone(),
            speed: self.speed,
            master: self.master,
        }
    }

    /// Makes instrument `inst` of bank `bank_id` playable here; returns its
    /// number in the session's bank. Instruments of the track's own bank
    /// need no copy.
    pub fn import(&mut self, bank_id: u16, bank: &Wd, inst: usize) -> usize {
        if bank_id == self.wd_orig.id {
            return inst;
        }
        let mut wd = (*self.wd).clone();
        let (n, base) = wd.import(bank, inst, self.imports.get(&bank_id).copied());
        self.imports.insert(bank_id, base);
        self.wd = Arc::new(wd);
        n
    }

    /// Sets a channel's instrument (`None`: the track's own).
    pub fn set_instrument(&mut self, ch: usize, pick: Option<(u16, &Wd, usize)>) {
        let v = pick.map(|(bank_id, bank, inst)| {
            // reuse an earlier import of the same instrument
            let known = self.channels.iter().filter_map(|c| c.instrument).find(|i| i.0 == bank_id && i.1 == inst);
            let n = match known {
                Some(k) => k.2,
                None => self.import(bank_id, bank, inst),
            };
            (bank_id, inst, n)
        });
        self.channels[ch].instrument = v;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seq::NoteSpec;

    fn session() -> Session {
        let note = |k: u8| vec![(0x11u8, vec![k, 100u8], 48u32), (0x1a, vec![k], 48)];
        let mut t1 = vec![(0x20u8, vec![3u8], 0u32)];
        t1.extend(note(60));
        t1.extend(note(62));
        t1.push((0x00, vec![], 0));
        let b = crate::seq::tests::bgm(0, &[(0, vec![(0x08, vec![120], 192), (0x00, vec![], 0)]), (0, t1)]);
        let wd = Arc::new(Wd { id: 1, instruments: Vec::new(), regions: Vec::new(), samples: Vec::new() });
        Session::new(1, "t".into(), &b, wd).unwrap()
    }

    #[test]
    fn undo_and_redo_note_edits_and_mixer() {
        let mut s = session();
        assert!(!s.can_undo());
        let before = s.seq.clone();
        s.seq.add_note(1, NoteSpec { start: 10, len: 5, key: 70, vel: 90 });
        s.seq_changed();
        s.checkpoint();
        let one = s.seq.clone();
        // a slider drag: many changes, one step
        for g in [0.9, 0.8, 0.7] {
            s.channels[1].gain = g;
        }
        s.checkpoint();
        s.checkpoint();
        assert!(s.can_undo());
        assert!(s.undo());
        assert_eq!(s.channels[1].gain, 1.0);
        assert_eq!(s.seq, one);
        assert!(s.undo());
        assert_eq!(s.seq, before);
        assert!(!s.undo());
        assert!(s.redo());
        assert_eq!(s.seq, one);
        assert!(s.redo());
        assert_eq!(s.channels[1].gain, 0.7);
        assert!(!s.redo());
        // a new edit drops the redo steps
        s.undo();
        s.channels[1].mute = true;
        s.checkpoint();
        assert!(!s.can_redo());
        // every restore is a new version (the player reloads)
        let v = s.version;
        s.undo();
        assert!(s.version > v);
    }

    #[test]
    fn reset_is_one_step_and_master_is_the_tracks() {
        let mut s = session();
        s.master = 0.5;
        s.tempo = 1.5;
        s.checkpoint();
        s.reset();
        assert_eq!((s.master, s.tempo), (1.0, 1.0));
        s.undo();
        assert_eq!((s.master, s.tempo), (0.5, 1.5));
    }

    #[test]
    fn program_of_a_channel() {
        let mut s = session();
        assert_eq!(s.program_at(1, 0), Some(3));
        assert_eq!(s.program_at(0, 0), None);
        s.channels[1].instrument = Some((9, 4, 7));
        assert_eq!(s.program_at(1, 0), Some(7));
    }
}
