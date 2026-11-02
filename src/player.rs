//! The playback path: the driver (`sqseq::Song`) on the chip (`Spu2`),
//! the converter's output stage (the +9 dB gain and limiter of
//! `musicjob::Master`), then the playback speed and the device's rate.
//! The audio thread, `--render` and the tests all play through [`Player`].

use std::sync::Arc;

use demaster::musicjob::Master;
use demaster::spu2::{self, Interp, Spu2};
use demaster::sqseq::{Bgm, Mods, Song, Tables, Wd};

/// The sound chip settings of the studio.
#[derive(Debug, Clone, PartialEq)]
pub struct Chip {
    /// Reverb mode (libsd: 0 off, 1-9 presets); `None`: the song's.
    pub reverb: Option<u8>,
    pub interp: Interp,
    pub dry: f32,
    pub wet: f32,
}

impl Default for Chip {
    fn default() -> Self {
        Chip { reverb: None, interp: Interp::Gaussian, dry: 1.0, wet: 1.0 }
    }
}

/// Everything the studio changes at playback.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub mods: Mods,
    pub chip: Chip,
    /// Playback speed (resampling: pitch goes with it).
    pub speed: f64,
    /// Master volume, before the limiter.
    pub master: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { mods: Mods::default(), chip: Chip::default(), speed: 1.0, master: 1.0 }
    }
}

/// The names of `libsd`'s reverb modes.
pub const REVERB_NAMES: [&str; 10] =
    ["off", "room", "studio A", "studio B", "studio C", "hall", "space", "echo", "delay", "pipe"];

pub struct Player {
    tab: Arc<Tables>,
    bgm: Arc<Bgm>,
    wd: Arc<Wd>,
    ram: Arc<Vec<u8>>,
    song: Song,
    spu: Spu2,
    settings: Settings,
    master: Master,
    /// Output rate of the device.
    out_rate: u32,
    // chip output after the master stage, waiting for the resampler
    pending: Vec<[i16; 2]>,
    pending_at: usize,
    /// Position of the resampler between pending[pending_at] and the next.
    frac: f64,
    scratch: Vec<[i16; 2]>,
    /// A note preview mixed into the output (already at output level).
    preview: Vec<[f32; 2]>,
    preview_at: usize,
    /// The chip's output as it is (no gain, no limiter, no master volume).
    pub raw: bool,
}

impl Player {
    pub fn new(tab: Arc<Tables>, bgm: Arc<Bgm>, wd: Arc<Wd>, out_rate: u32) -> Self {
        let ram = Arc::new(wd.samples.clone());
        let mut spu = Spu2::with_ram(ram.clone());
        let song = Song::new(tab.clone(), bgm.clone(), wd.clone(), &mut spu);
        let mut p = Player {
            tab,
            bgm,
            wd,
            ram,
            song,
            spu,
            settings: Settings::default(),
            master: Master::new(),
            out_rate,
            pending: Vec::new(),
            pending_at: 0,
            frac: 0.0,
            scratch: Vec::new(),
            preview: Vec::new(),
            preview_at: 0,
            raw: false,
        };
        p.apply_chip(true);
        p
    }

    pub fn song(&self) -> &Song {
        &self.song
    }

    pub fn spu(&self) -> &Spu2 {
        &self.spu
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Back to the start of the song (settings stay).
    pub fn restart(&mut self) {
        self.spu = Spu2::with_ram(self.ram.clone());
        self.song = Song::new(self.tab.clone(), self.bgm.clone(), self.wd.clone(), &mut self.spu);
        self.song.set_mods(self.settings.mods.clone());
        self.master = Master::new();
        self.pending.clear();
        self.pending_at = 0;
        self.frac = 0.0;
        self.apply_chip(true);
    }

    fn apply_chip(&mut self, fresh: bool) {
        let c = &self.settings.chip;
        self.spu.interp = c.interp;
        self.spu.dry = c.dry;
        self.spu.wet = c.wet;
        let mode = c.reverb.map(usize::from);
        match mode {
            Some(0) => self.spu.reverb_off(),
            Some(m) if m < self.tab.reverb.len() => {
                self.spu.set_reverb(0, self.tab.reverb[m]);
                self.spu.set_reverb(1, self.tab.reverb[m]);
            }
            // back to the song's own: Song::new set it up
            _ if !fresh => {
                let m = usize::from(self.bgm.reverb_mode);
                if m != 0 && m < self.tab.reverb.len() {
                    self.spu.set_reverb(0, self.tab.reverb[m]);
                    self.spu.set_reverb(1, self.tab.reverb[m]);
                } else {
                    self.spu.reverb_off();
                }
            }
            _ => {}
        }
    }

    pub fn set_settings(&mut self, s: Settings) {
        let chip_changed = s.chip.reverb != self.settings.chip.reverb;
        if s.mods != self.settings.mods {
            self.song.set_mods(s.mods.clone());
        }
        self.settings = s;
        self.apply_chip(!chip_changed);
    }

    /// A new version of the sequence; playback goes on from the same tick.
    pub fn set_bgm(&mut self, bgm: Arc<Bgm>) {
        let tick = self.song.tick;
        self.bgm = bgm;
        self.restart();
        self.seek_tick(tick);
    }

    /// A bank with instruments appended (the old ones at the same place).
    pub fn set_wd(&mut self, wd: Arc<Wd>) {
        self.ram = Arc::new(wd.samples.clone());
        self.spu.set_ram(self.ram.clone());
        self.song.set_wd(wd.clone());
        self.wd = wd;
    }

    /// Plays on silently to `tick` of the sequence (from the start).
    pub fn seek_tick(&mut self, tick: u32) {
        if tick < self.song.tick || self.song.frame() > 0 && tick == 0 {
            self.restart();
        }
        let limit = self.song.frame() + u64::from(spu2::RATE) * 3600;
        while self.song.tick < tick && self.song.playing() && self.song.frame() < limit {
            self.song.interrupt_skip(&mut self.spu);
        }
        self.pending.clear();
        self.pending_at = 0;
        self.master = Master::new();
    }

    /// Whether anything is left to hear.
    pub fn finished(&self) -> bool {
        !self.song.playing() && !self.spu.busy() && self.preview_at >= self.preview.len()
    }

    /// Mixes a short sound (chip level, 48 kHz) into the output.
    pub fn play_preview(&mut self, chip: &[[i16; 2]]) {
        let g = demaster::musicjob::gain() * self.settings.master;
        self.preview = chip.iter().map(|s| [f32::from(s[0]) * g, f32::from(s[1]) * g]).collect();
        self.preview_at = 0;
    }

    /// One interrupt of the chip through the master stage into `pending`.
    fn produce(&mut self) {
        self.scratch.clear();
        self.song.interrupt(&mut self.spu, &mut self.scratch);
        if self.pending_at > 0 {
            self.pending.drain(..self.pending_at);
            self.pending_at = 0;
        }
        if self.raw {
            self.pending.extend_from_slice(&self.scratch);
            return;
        }
        let v = self.settings.master;
        for s in &self.scratch {
            self.master.frame([f32::from(s[0]) * v, f32::from(s[1]) * v], &mut self.pending);
        }
    }

    /// `n` output frames at the device's rate (interleaved stereo f32 in
    /// -1..1 appended to `out`).
    pub fn render(&mut self, n: usize, out: &mut Vec<[f32; 2]>) {
        let step = self.settings.speed.clamp(0.1, 4.0) * f64::from(spu2::RATE) / f64::from(self.out_rate);
        for _ in 0..n {
            while self.pending_at + 1 >= self.pending.len() {
                self.produce();
            }
            let a = self.pending[self.pending_at];
            let b = self.pending[self.pending_at + 1];
            let f = self.frac as f32;
            let mut x = [
                f32::from(a[0]) + (f32::from(b[0]) - f32::from(a[0])) * f,
                f32::from(a[1]) + (f32::from(b[1]) - f32::from(a[1])) * f,
            ];
            if let Some(p) = self.preview.get(self.preview_at) {
                x[0] += p[0];
                x[1] += p[1];
                self.preview_at += 1;
            }
            out.push([(x[0] / 32768.0).clamp(-1.0, 1.0), (x[1] / 32768.0).clamp(-1.0, 1.0)]);
            self.frac += step;
            let whole = self.frac.floor();
            self.frac -= whole;
            self.pending_at += whole as usize;
        }
    }

    /// `n` frames as 16-bit (the WAV path; equal to [`Player::render`] at
    /// speed 1 and 48 kHz).
    pub fn render_i16(&mut self, n: usize, out: &mut Vec<[i16; 2]>) {
        let mut f = Vec::with_capacity(n);
        self.render(n, &mut f);
        out.extend(f.iter().map(|s| [(s[0] * 32768.0).round().clamp(-32768.0, 32767.0) as i16, (s[1] * 32768.0).round().clamp(-32768.0, 32767.0) as i16]));
    }
}

/// One note of an instrument, for listening to it: a one-track sequence
/// (tempo 120, the instrument, the key for `beats`, the end) played by the
/// driver without reverb. Chip level, 48 kHz.
pub fn preview(tab: &Arc<Tables>, wd: &Arc<Wd>, inst: usize, key: u8, beats: u32) -> Vec<[i16; 2]> {
    let mut b = vec![0u8; 0x20];
    b[..4].copy_from_slice(b"BGM ");
    b[6..8].copy_from_slice(&wd.id.to_le_bytes());
    b[8] = 1;
    b[9] = 0xff;
    b[12] = 0x7f;
    b[0xe..0x10].copy_from_slice(&48u16.to_le_bytes());
    let len = 48 * beats.max(1);
    let mut t = vec![0u8, 0x08, 120, 0, 0x20, inst as u8, 0, 0x11, key, 100];
    let mut d = Vec::new();
    let mut v = len;
    d.push((v & 0x7f) as u8);
    v >>= 7;
    while v != 0 {
        d.insert(0, (v & 0x7f) as u8 | 0x80);
        v >>= 7;
    }
    t.extend(d);
    t.extend([0x1a, key, 0, 0x00]);
    b.extend_from_slice(&(t.len() as u32).to_le_bytes());
    b.extend(t);
    let n = b.len() as u32;
    b[0x10..0x14].copy_from_slice(&n.to_le_bytes());
    let Ok(bgm) = Bgm::parse(&b) else { return Vec::new() };
    let mut spu = Spu2::new(wd.samples.clone());
    let mut song = Song::new(tab.clone(), Arc::new(bgm), wd.clone(), &mut spu);
    let mut out = Vec::new();
    let max = spu2::RATE as usize * (beats as usize + 3);
    while out.len() < max && (song.playing() || spu.busy()) {
        song.interrupt(&mut spu, &mut out);
    }
    out
}
