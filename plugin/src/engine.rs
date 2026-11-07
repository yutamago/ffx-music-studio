//! The plugin's processing, without the plugin API: MIDI in host blocks to
//! the PS2 driver (`sqseq::Song::external`) on its SPU2 (`spu2::Spu2`),
//! then the studio's output stage (`musicjob::Master`). The plugin's
//! `process()` calls [`Engine::process`]; so do the tests and
//! `examples/render_midi.rs`.
//!
//! Timing. The driver plays its own 240 Hz interrupts and its tempo counter
//! (tempo source "sequence") or ticks counted from the host's position
//! ("host"). An event's tick is the host's position at the event, in the
//! MIDI file's 48 ticks a quarter note; its commands run in the driver's
//! tick of that number. The song starts [`Engine::latency`] after the
//! transport, so every event is there before its tick is played (the
//! driver plays a tick in the interrupt after its exact time; a DAW plays
//! the event at the exact time). Hosts compensate the reported latency.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use demaster::musicjob::Master;
use demaster::spu2::{self, Spu2};
use demaster::sqseq::{ExtHeader, FRAMES_PER_IRQ, Song, Tables, Wd};

use crate::midi::{Batch, Midi, Translator};

/// MIDI channels: one driver track each.
pub const TRACKS: usize = crate::midi::TRACKS;
/// The chip's voices (48: both SPU2 cores).
pub const VOICES: usize = demaster::spu2::VOICES;
/// Ticks a quarter note (the MIDI export's PPQN).
pub const PPQN: f64 = 48.0;
/// How far the song runs behind the host (seconds).
pub const LOOKAHEAD_S: f64 = 0.025;
/// The output stage's own delay (chip frames).
pub const STAGE_DELAY: usize = 96;

/// One bank and what goes with it.
pub struct Instrument {
    pub tables: Arc<Tables>,
    pub wd: Arc<Wd>,
    pub ram: Arc<Vec<u8>>,
    /// The track's header (song volume, reverb), or the defaults.
    pub header: ExtHeader,
    pub label: String,
}

impl Instrument {
    pub fn new(tables: Arc<Tables>, wd: Arc<Wd>, header: ExtHeader, label: String) -> Self {
        let ram = Arc::new(wd.samples.clone());
        Instrument { tables, wd, ram, header, label }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempoSource {
    /// The song's tempo commands (CC 23) drive the driver's own counter:
    /// the PS2's timing exactly.
    Sequence,
    /// The host's position: the DAW's tempo.
    Host,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Settings {
    pub tempo: TempoSource,
    /// The studio's output stage (+9 dB, limiter at -1 dBFS) as in the
    /// reference renders; off: the chip's level.
    pub stage: bool,
    pub gain: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { tempo: TempoSource::Sequence, stage: true, gain: 1.0 }
    }
}

/// The host's transport at the start of a block.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Transport {
    pub playing: bool,
    /// Position in quarter notes.
    pub ppq: Option<f64>,
    pub tempo: Option<f64>,
}

/// A MIDI message `at` samples into the block.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Event {
    pub at: u32,
    pub msg: Midi,
}

/// Where the driver's ticks are against the host's.
#[derive(Debug, Clone, Copy)]
struct Clock {
    /// Host tick of the driver's tick 0.
    origin: i64,
    /// Added at jumps of the host (loops, seeks): the driver goes on.
    offset: i64,
    /// The host's position expected at the next block.
    next_ppq: Option<f64>,
}

pub struct Engine {
    inst: Arc<Instrument>,
    sr: f64,
    /// Chip frames a host sample.
    step: f64,
    pub settings: Settings,
    song: Song,
    spu: Spu2,
    master: Master,
    header: ExtHeader,
    tr: Translator,
    /// By driver tick: the channels' events.
    batches: BTreeMap<u64, Vec<Batch>>,
    /// Host samples done.
    host_pos: u64,
    /// Chip frames made; `out` holds frames `out_base..chip_end`.
    chip_end: u64,
    out: VecDeque<[f32; 2]>,
    out_base: u64,
    read: f64,
    next_irq: u64,
    start_at: Option<u64>,
    clock: Option<Clock>,
    playing: bool,
    /// Recent blocks: host sample, position, tempo (the "host" clock).
    hist: VecDeque<(u64, f64, f64)>,
    /// Tempo to start with (the host's, before the song's own).
    start_bpm: u8,
    scratch: Vec<[i16; 2]>,
    staged: Vec<[i16; 2]>,
    /// Events that came after their tick had been played.
    pub late: u64,
    /// How far the song runs behind the host (host samples).
    lookahead: u64,
    /// The output stage's delay when the stage is off.
    raw_delay: VecDeque<[i16; 2]>,
}

fn fresh_master() -> Master {
    let mut m = Master::new();
    let mut sink = Vec::new();
    for _ in 0..STAGE_DELAY {
        m.frame([0.0, 0.0], &mut sink);
    }
    m
}

impl Engine {
    pub fn new(inst: Arc<Instrument>, sample_rate: f64, settings: Settings) -> Self {
        let mut spu = Spu2::with_ram(inst.ram.clone());
        let song = Song::external(inst.tables.clone(), inst.wd.clone(), TRACKS, inst.header, &mut spu);
        let sr = if sample_rate > 0.0 { sample_rate } else { f64::from(spu2::RATE) };
        Engine {
            header: inst.header,
            inst,
            sr,
            step: f64::from(spu2::RATE) / sr,
            settings,
            song,
            spu,
            master: fresh_master(),
            tr: Translator::default(),
            batches: BTreeMap::new(),
            host_pos: 0,
            chip_end: 0,
            out: VecDeque::new(),
            out_base: 0,
            read: 0.0,
            next_irq: 0,
            start_at: None,
            clock: None,
            playing: false,
            hist: VecDeque::new(),
            start_bpm: 120,
            scratch: Vec::new(),
            staged: Vec::new(),
            late: 0,
            lookahead: (LOOKAHEAD_S * sr).round() as u64,
            raw_delay: VecDeque::from(vec![[0i16; 2]; STAGE_DELAY]),
        }
    }

    /// At least the default lookahead and two host blocks (an event comes
    /// with its block, at most a block late from a linked part).
    pub fn set_max_block(&mut self, max_block: usize) {
        self.lookahead = ((LOOKAHEAD_S * self.sr).round() as u64).max(2 * max_block as u64);
    }

    /// An event from a linked instance (another part): its host tick, or
    /// `None` (transport stopped) for the next tick.
    pub fn push_host(&mut self, host_tick: Option<i64>, msg: Midi) {
        let ch = usize::from(msg.channel()) % TRACKS;
        let tick = match (&self.clock, host_tick) {
            (Some(c), Some(h)) => (h - c.origin + c.offset).max(0) as u64,
            _ => self.song.ticks_run(),
        };
        self.batch(tick, ch).push(msg);
    }

    pub fn instrument(&self) -> &Arc<Instrument> {
        &self.inst
    }

    /// Host samples from an event to its sound (song start to output).
    pub fn latency(&self) -> u32 {
        let stage = STAGE_DELAY as f64 / self.step;
        (self.lookahead() as f64 + stage).round() as u32
    }

    fn lookahead(&self) -> u64 {
        self.lookahead
    }

    /// The driver and the chip (checks).
    pub fn song(&self) -> &Song {
        &self.song
    }

    pub fn spu(&self) -> &Spu2 {
        &self.spu
    }

    /// Voices sounding (of 48).
    pub fn voices(&self) -> usize {
        self.song.voice_tracks().iter().filter(|t| t.is_some()).count()
    }

    fn reset_song(&mut self) {
        self.spu = Spu2::with_ram(self.inst.ram.clone());
        self.header = self.inst.header;
        self.song = Song::external(self.inst.tables.clone(), self.inst.wd.clone(), TRACKS, self.header, &mut self.spu);
        self.master = fresh_master();
        self.raw_delay = VecDeque::from(vec![[0i16; 2]; STAGE_DELAY]);
        self.tr = Translator::default();
        // the host's tempo until the song's own (the driver would start at
        // 1 BPM)
        self.song.ext_command(0, 0, &[0x08, self.start_bpm]);
    }

    /// The host tick at host sample `at` (from the block positions seen).
    fn host_tick_at(&self, at: f64) -> Option<f64> {
        let &(s, ppq, tempo) = self.hist.iter().rev().find(|h| h.0 as f64 <= at).or(self.hist.front())?;
        Some((ppq + (at - s as f64) / self.sr * tempo / 60.0) * PPQN)
    }

    fn batch(&mut self, tick: u64, ch: usize) -> &mut Batch {
        let v = self.batches.entry(tick).or_insert_with(|| vec![Batch::default(); TRACKS]);
        &mut v[ch]
    }

    /// One block: the host's transport at its start, its MIDI events
    /// (sorted by `at`), and the output (both channels the same length).
    pub fn process(&mut self, tr: &Transport, events: &[Event], left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        let tempo = tr.tempo.unwrap_or(120.0).max(1.0);
        if let Some(ppq) = tr.ppq {
            self.hist.push_back((self.host_pos, ppq, tempo));
            while self.hist.len() > 256 {
                self.hist.pop_front();
            }
        }
        // transport changes
        if tr.playing && !self.playing {
            let ppq = tr.ppq.unwrap_or(0.0);
            self.clock = Some(Clock { origin: (ppq * PPQN).round() as i64, offset: 0, next_ppq: None });
            self.batches.clear();
            self.start_bpm = tempo.round().clamp(1.0, 255.0) as u8;
            let at = ((self.host_pos + self.lookahead()) as f64 * self.step).round() as u64;
            self.start_at = Some(at.max(self.chip_end));
        } else if !tr.playing && self.playing {
            self.clock = None;
            self.start_at = None;
            self.batches.clear();
            self.song.ext_clear();
            self.song.release_all(&mut self.spu);
        }
        self.playing = tr.playing;
        if let (Some(c), Some(ppq)) = (&mut self.clock, tr.ppq) {
            // a jump (loop, seek): the driver goes on from where it is
            if let Some(want) = c.next_ppq
                && ((ppq - want) * PPQN).abs() > 0.5
            {
                c.offset += (want * PPQN).round() as i64 - (ppq * PPQN).round() as i64;
            }
            c.next_ppq = Some(ppq + n as f64 / self.sr * tempo / 60.0);
        }
        // events to their ticks
        for e in events {
            let ch = usize::from(e.msg.channel()) % TRACKS;
            let tick = match (&self.clock, tr.ppq) {
                (Some(c), Some(ppq)) => {
                    let h = ((ppq + f64::from(e.at) / self.sr * tempo / 60.0) * PPQN).round() as i64;
                    (h - c.origin + c.offset).max(0) as u64
                }
                // live: the next tick
                _ => self.song.ticks_run(),
            };
            self.batch(tick, ch).push(e.msg);
        }
        // the output
        for i in 0..n {
            let pos = self.read;
            let need = pos.floor() as u64 + if self.step == 1.0 { 1 } else { 2 };
            self.produce(need);
            let k = (pos.floor() as u64 - self.out_base) as usize;
            let a = self.out[k];
            let x = if self.step == 1.0 {
                a
            } else {
                let b = self.out[k + 1];
                let f = (pos - pos.floor()) as f32;
                [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]
            };
            left[i] = x[0];
            right[i] = x[1];
            self.read += self.step;
        }
        let keep = self.read.floor() as u64;
        while self.out_base < keep && !self.out.is_empty() {
            self.out.pop_front();
            self.out_base += 1;
        }
        self.host_pos += n as u64;
    }

    /// Chip frames up to `upto`.
    fn produce(&mut self, upto: u64) {
        while self.chip_end < upto {
            if self.start_at == Some(self.chip_end) {
                self.start_at = None;
                self.reset_song();
                self.next_irq = self.chip_end;
            }
            if self.chip_end == self.next_irq {
                self.irq();
                self.next_irq += FRAMES_PER_IRQ as u64;
            }
            let lim = self.next_irq.min(self.start_at.unwrap_or(u64::MAX));
            let k = (lim - self.chip_end) as usize;
            self.scratch.clear();
            self.spu.render(k, &mut self.scratch);
            let g = self.settings.gain / 32768.0;
            if self.settings.stage {
                self.staged.clear();
                for s in &self.scratch {
                    self.master.frame([f32::from(s[0]), f32::from(s[1])], &mut self.staged);
                }
                self.out.extend(self.staged.iter().map(|s| [f32::from(s[0]) * g, f32::from(s[1]) * g]));
            } else {
                // the same delay as the stage's, so the latency stays
                for s in &self.scratch {
                    self.raw_delay.push_back(*s);
                    let y = self.raw_delay.pop_front().unwrap_or_default();
                    self.out.push_back([f32::from(y[0]) * g, f32::from(y[1]) * g]);
                }
            }
            self.chip_end += k as u64;
        }
    }

    /// One interrupt: the due events into the driver, then its timer.
    fn irq(&mut self) {
        // waiting for the song to start: the old one plays out
        let playing = self.clock.is_some() && self.start_at.is_none();
        let now = self.song.ticks_run();
        let due: Vec<u64> = if self.start_at.is_some() {
            Vec::new()
        } else {
            self.batches.range(..=now + 4).map(|(t, _)| *t).collect()
        };
        for tick in due {
            let Some(bs) = self.batches.remove(&tick) else { continue };
            for (ch, b) in bs.iter().enumerate() {
                if b.is_empty() {
                    continue;
                }
                if tick < now {
                    self.late += 1;
                }
                let c = self.tr.commands(ch, b, self.header);
                if let Some(h) = c.header {
                    self.song.ext_set_header(&mut self.spu, h);
                    self.header = h;
                }
                if c.sound_off {
                    self.song.release_all(&mut self.spu);
                }
                for cmd in &c.cmds {
                    self.song.ext_command(tick, ch, cmd);
                }
            }
        }
        let ticks = match (playing, self.settings.tempo) {
            (false, _) => Some(1),
            (true, TempoSource::Sequence) => None,
            (true, TempoSource::Host) => {
                let c = self.clock.expect("clock");
                // the host's position for this interrupt, `lookahead` ago
                let at = self.next_irq as f64 / self.step - self.lookahead() as f64;
                let h = self.host_tick_at(at).map_or(0.0, |h| h - c.origin as f64 + c.offset as f64);
                let target = h.floor().max(-1.0) as i64 + 1;
                Some((target - self.song.ticks_run() as i64).clamp(0, 8) as u32)
            }
        };
        self.song.ext_irq(&mut self.spu, ticks);
    }
}
