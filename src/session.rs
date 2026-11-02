//! One loaded track with its edits (in memory only): the sequence, the
//! mixer, the imported instruments, the chip and loop settings. The UI and
//! `--render` both edit through it; [`Session::settings`] and
//! [`Session::bgm`] are what the player gets.

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
    pub master: f32,
    /// Loop at the end (else stop).
    pub looping: bool,
    /// The studio's loop range (ticks), `None`: the song's own loop.
    pub loop_range: Option<(u32, u32)>,
    /// Counts sequence changes (the player reloads when it moves).
    pub version: u64,
}

impl Session {
    pub fn new(id: u16, label: String, bytes: &[u8], wd: Arc<Wd>) -> Result<Session> {
        let seq = Seq::decode(bytes)?;
        let n = seq.tracks.len();
        Ok(Session {
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
        })
    }

    /// Back to the track as on the disc.
    pub fn reset(&mut self) {
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

    pub fn settings(&self) -> Settings {
        let solo = self.channels.iter().any(|c| c.solo);
        let tracks = self
            .channels
            .iter()
            .map(|c| TrackMod {
                gain: if c.mute || (solo && !c.solo) { 0.0 } else { c.gain },
                pan: c.pan,
                transpose: c.transpose,
                program: c.instrument.map(|i| i.2),
            })
            .collect::<Vec<_>>();
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
