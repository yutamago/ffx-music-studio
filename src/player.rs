//! The playback path: the driver (`sqseq::Song`) on the chip (`Spu2`),
//! the converter's output stage (the +9 dB gain and limiter of
//! `musicjob::Master`), then the playback speed and the device's rate.
//! The audio thread, `--render` and the tests all play through [`Player`].
//! [`Live`] plays the notes of the piano roll's keys and a MIDI keyboard.

use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use demaster::musicjob::Master;
use demaster::spu2::{self, Interp, Spu2};
use demaster::sqseq::{Bgm, FRAMES_PER_IRQ, Mods, Song, Tables, TrackMod, Wd};

use crate::seq::PPQN;

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

/// Frames of silence that end a fade (1/3 s), and the longest fade (20 s).
const QUIET_END: usize = spu2::RATE as usize / 3;
const FADE_MAX: usize = spu2::RATE as usize * 20;
/// A metronome click: 25 ms.
const CLICK_LEN: usize = 1200;

/// Each track's loudness now (the levels of the voices playing for it),
/// kept if louder than what `into` holds.
fn track_levels(song: &Song, spu: &Spu2, into: &mut Vec<f32>) {
    let vt = song.voice_tracks();
    let n = song.programs().len();
    if into.len() < n {
        into.resize(n, 0.0);
    }
    let mut cur = vec![0f32; n];
    for (i, t) in vt.iter().enumerate() {
        if let Some(t) = *t
            && t < n
        {
            cur[t] += spu.voice_level(i);
        }
    }
    for (a, b) in into.iter_mut().zip(cur) {
        *a = a.max(b);
    }
}

#[derive(Clone)]
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
    /// Paused: every note was released and the chip plays on (releases,
    /// reverb tail) while the sequence stands still.
    frozen: bool,
    /// Frames the chip has been silent while frozen, and frozen in all.
    quiet: usize,
    fade_frames: usize,
    /// Clicks on the beats (the window only; not `--render`, not the export).
    pub metronome: bool,
    last_beat: Option<u32>,
    /// A click being played: frame in it, accented (first beat of a bar).
    click: Option<(usize, bool)>,
    /// The loudest each track was since [`Player::take_levels`].
    levels: Vec<f32>,
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
            frozen: false,
            quiet: 0,
            fade_frames: 0,
            metronome: false,
            last_beat: None,
            click: None,
            levels: Vec::new(),
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

    /// The sound RAM (shared with [`Live`]).
    pub fn ram(&self) -> Arc<Vec<u8>> {
        self.ram.clone()
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
        self.frozen = false;
        self.last_beat = None;
        self.click = None;
        self.apply_chip(true);
    }

    /// Pause: every sounding note goes into its own release (the SPU2's
    /// ADSR); [`Player::render`] then plays the chip on, releases and reverb
    /// tail, with the sequence standing still, until [`Player::fading`]
    /// ends. [`Player::resume`] goes on from the same tick.
    pub fn pause(&mut self) {
        if !self.frozen {
            self.song.release_all(&mut self.spu);
            self.frozen = true;
            self.quiet = 0;
            self.fade_frames = 0;
            self.click = None;
        }
    }

    pub fn resume(&mut self) {
        self.frozen = false;
    }

    #[allow(dead_code)]
    pub fn paused(&self) -> bool {
        self.frozen
    }

    /// Paused and still sounding (releases or the reverb tail).
    pub fn fading(&self) -> bool {
        self.frozen && self.quiet < QUIET_END && self.fade_frames < FADE_MAX
    }

    /// Stop: the sounding notes fade out in a copy (returned: play it while
    /// it is [`Player::fading`]); this one goes back to the start.
    pub fn stop_with_tail(&mut self) -> Player {
        let mut t = self.clone();
        t.preview.clear();
        t.preview_at = 0;
        t.pause();
        self.restart();
        t
    }

    /// The loudest each track (channel) was since the last call, 0..1.
    pub fn take_levels(&mut self) -> Vec<f32> {
        let n = self.song.programs().len();
        let mut v = std::mem::replace(&mut self.levels, vec![0.0; n]);
        v.resize(n, 0.0);
        v
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
        if self.frozen || tick < self.song.tick || self.song.frame() > 0 && tick == 0 {
            self.restart();
        }
        let limit = self.song.frame() + u64::from(spu2::RATE) * 3600;
        while self.song.tick < tick && self.song.playing() && self.song.frame() < limit {
            self.song.interrupt_skip(&mut self.spu);
        }
        self.pending.clear();
        self.pending_at = 0;
        self.master = Master::new();
        self.last_beat = None;
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

    /// One interrupt of the chip through the master stage into `pending`
    /// (frozen: the chip alone, the sequence stands still).
    fn produce(&mut self) {
        self.scratch.clear();
        if self.frozen {
            self.spu.render(FRAMES_PER_IRQ, &mut self.scratch);
            self.fade_frames += FRAMES_PER_IRQ;
            let loud = self.scratch.iter().any(|s| s[0].unsigned_abs() > 2 || s[1].unsigned_abs() > 2);
            self.quiet = if loud || self.spu.busy() { 0 } else { self.quiet + FRAMES_PER_IRQ };
        } else {
            self.song.interrupt(&mut self.spu, &mut self.scratch);
        }
        track_levels(&self.song, &self.spu, &mut self.levels);
        if self.pending_at > 0 {
            self.pending.drain(..self.pending_at);
            self.pending_at = 0;
        }
        let from = self.pending.len();
        if self.raw {
            self.pending.extend_from_slice(&self.scratch);
        } else {
            let v = self.settings.master;
            for s in &self.scratch {
                self.master.frame([f32::from(s[0]) * v, f32::from(s[1]) * v], &mut self.pending);
            }
        }
        if self.metronome && !self.frozen && self.song.playing() {
            let beat = self.song.tick.saturating_sub(1) / PPQN;
            if self.song.tick > 0 && self.last_beat != Some(beat) {
                self.click = Some((0, beat % 4 == 0));
            }
            self.last_beat = Some(beat);
        }
        if let Some((mut at, accent)) = self.click {
            for f in &mut self.pending[from..] {
                if at >= CLICK_LEN {
                    break;
                }
                let x = click_sample(at, accent);
                f[0] = (i32::from(f[0]) + x).clamp(-32768, 32767) as i16;
                f[1] = (i32::from(f[1]) + x).clamp(-32768, 32767) as i16;
                at += 1;
            }
            self.click = (at < CLICK_LEN).then_some((at, accent));
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

/// Frame `i` of a metronome click (a decaying sine, higher on the bar).
pub fn click_sample(i: usize, accent: bool) -> i32 {
    let f = if accent { 1760.0 } else { 1320.0 };
    let t = i as f32 / spu2::RATE as f32;
    let env = (1.0 - i as f32 / CLICK_LEN as f32).max(0.0).powi(3);
    ((t * f * std::f32::consts::TAU).sin() * env * 9000.0) as i32
}

/// Renders a track once through (the song's loop not taken: to its end,
/// then 2 s of release and reverb) into a 48 kHz WAV. Returns the seconds.
pub fn export_wav(tab: Arc<Tables>, bgm: Arc<Bgm>, wd: Arc<Wd>, mut settings: Settings, path: &Path, max_seconds: f64) -> Result<f64> {
    settings.mods.no_loop = true;
    let mut p = Player::new(tab, bgm, wd, spu2::RATE);
    p.set_settings(settings);
    p.restart();
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let mut w = demaster::musicjob::WavWriter::create(path, spu2::RATE)?;
    let mut buf = Vec::new();
    let (mut frames, mut tail) = (0usize, 0usize);
    let max = (max_seconds * f64::from(spu2::RATE)) as usize;
    while frames < max && tail < 2 * spu2::RATE as usize {
        buf.clear();
        p.render_i16(4800, &mut buf);
        w.write(&buf)?;
        frames += buf.len();
        if p.finished() {
            tail += buf.len();
        }
    }
    w.finish()?;
    Ok(frames as f64 / f64::from(spu2::RATE))
}

/// Notes played live (the piano roll's keys, a MIDI keyboard) on a
/// channel's instrument: a driver of its own whose tracks (one per channel)
/// wait and never end; a note is a key-on of the channel's track with the
/// channel's instrument, volume, pan and transpose ([`TrackMod`]). Its own
/// chip on the same sound RAM, no reverb, mixed into the output.
pub struct Live {
    song: Song,
    spu: Spu2,
    mods: Mods,
    out_rate: u32,
    pending: Vec<[i16; 2]>,
    at: usize,
    frac: f64,
    idle: bool,
    levels: Vec<f32>,
}

impl Live {
    pub fn new(tab: Arc<Tables>, wd: Arc<Wd>, ram: Arc<Vec<u8>>, channels: usize, out_rate: u32) -> Live {
        let channels = channels.clamp(1, 64);
        let mut b = vec![0u8; 0x20];
        b[..4].copy_from_slice(b"BGM ");
        b[6..8].copy_from_slice(&wd.id.to_le_bytes());
        b[8] = channels as u8;
        b[9] = 0xff;
        b[12] = 0x7f;
        b[0xe..0x10].copy_from_slice(&48u16.to_le_bytes());
        for _ in 0..channels {
            // the longest wait, then the end
            let t = [0xffu8, 0xff, 0xff, 0x7f, 0x00];
            b.extend_from_slice(&(t.len() as u32).to_le_bytes());
            b.extend_from_slice(&t);
        }
        let n = b.len() as u32;
        b[0x10..0x14].copy_from_slice(&n.to_le_bytes());
        let bgm = Arc::new(Bgm::parse(&b).expect("live sequence"));
        let mut spu = Spu2::with_ram(ram);
        let song = Song::new(tab, bgm, wd, &mut spu);
        Live { song, spu, mods: Mods::default(), out_rate, pending: Vec::new(), at: 0, frac: 0.0, idle: true, levels: vec![0.0; channels] }
    }

    pub fn note_on(&mut self, ch: usize, key: u8, vel: u8, m: TrackMod) {
        if self.mods.tracks.len() <= ch {
            self.mods.tracks.resize(ch + 1, TrackMod::default());
        }
        if self.mods.tracks[ch] != m {
            self.mods.tracks[ch] = m;
            self.song.set_mods(self.mods.clone());
        }
        self.song.live_key_on(ch, key, vel);
        self.idle = false;
    }

    pub fn note_off(&mut self, ch: usize, key: u8) {
        self.song.live_key_off(ch, key);
    }

    pub fn bend(&mut self, ch: usize, raw: i32) {
        self.song.live_bend(ch, raw);
    }

    pub fn all_off(&mut self) {
        self.song.release_all(&mut self.spu);
    }

    /// Something sounds (or is about to).
    pub fn active(&self) -> bool {
        !self.idle
    }

    /// Adds `out.len()` frames at the output rate, times `gain` (chip level
    /// to -1..1).
    pub fn mix(&mut self, out: &mut [[f32; 2]], gain: f32) {
        let step = f64::from(spu2::RATE) / f64::from(self.out_rate);
        for o in out.iter_mut() {
            while self.at + 1 >= self.pending.len() {
                if self.at > 0 {
                    self.pending.drain(..self.at);
                    self.at = 0;
                }
                self.song.interrupt(&mut self.spu, &mut self.pending);
                track_levels(&self.song, &self.spu, &mut self.levels);
                if !self.spu.busy() {
                    self.idle = true;
                }
            }
            let (a, b) = (self.pending[self.at], self.pending[self.at + 1]);
            let f = self.frac as f32;
            o[0] += (f32::from(a[0]) + (f32::from(b[0]) - f32::from(a[0])) * f) * gain;
            o[1] += (f32::from(a[1]) + (f32::from(b[1]) - f32::from(a[1])) * f) * gain;
            self.frac += step;
            let whole = self.frac.floor();
            self.frac -= whole;
            self.at += whole as usize;
        }
    }

    pub fn take_levels(&mut self) -> Vec<f32> {
        let n = self.levels.len();
        std::mem::replace(&mut self.levels, vec![0.0; n])
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
