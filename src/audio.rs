//! Real-time output. An engine thread owns the [`Player`] and keeps a small
//! ring buffer (about 40 ms) filled; the device callback only copies from
//! the ring (silence when it runs dry), so the UI never blocks the sound and
//! the sound never waits for the UI. Without a sound device (or in a Linux
//! build without the `alsa` feature) a clock thread drains the ring at the
//! real rate, so the position still moves.
//!
//! The output is the sum of: the song (playing, or fading out after a
//! pause), the tail of a stopped song, the live notes ([`Live`]) and an
//! instrument preview.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use demaster::sqseq::{Bgm, Tables, TrackMod, Wd};
use ringbuf::HeapRb;
use ringbuf::traits::{Consumer, Observer, Producer, Split};

use crate::player::{Live, Player, Settings};

/// A note played live on a channel (the piano roll's keys, MIDI).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LiveCmd {
    /// `m`: the channel's instrument, volume, pan and transpose.
    On { ch: usize, key: u8, vel: u8, m: TrackMod },
    Off { ch: usize, key: u8 },
    /// -0x2000..0x1fff
    Bend { ch: usize, raw: i32 },
    AllOff,
}

pub enum Cmd {
    Load { tab: Arc<Tables>, bgm: Arc<Bgm>, wd: Arc<Wd>, settings: Settings },
    Play,
    /// Pause: the notes fade out (their releases and the reverb), the
    /// sequence stays where it is.
    Pause,
    /// Back to the start; the notes fade out. During a fade: cut at once.
    Stop,
    SeekTick(u32),
    Settings(Settings),
    Bgm(Arc<Bgm>),
    Wd(Arc<Wd>),
    Preview(Vec<[i16; 2]>),
    Live(LiveCmd),
    Metronome(bool),
    /// The window's output volume (after everything else).
    Output(f32),
}

/// What the UI shows; written by the engine thread.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub loaded: bool,
    pub playing: bool,
    pub finished: bool,
    /// Paused (or stopped) and still sounding.
    pub fading: bool,
    /// Chip frames since the start (48 kHz, loops included).
    pub frame: u64,
    pub tick: u32,
    pub bpm: i32,
    pub voices: Vec<u32>,
    /// Per channel the loudest its voices were since the last status
    /// (song and live notes), 0..1.
    pub levels: Vec<f32>,
    pub programs: Vec<Option<usize>>,
    pub active_voices: usize,
    pub loops: usize,
    pub peak: [f32; 2],
    /// Last seek or reload took this long (ms).
    pub busy_ms: u32,
}

pub struct Engine {
    tx: Sender<Cmd>,
    pub status: Arc<Mutex<Status>>,
    pub device: String,
    // keeps the device stream alive
    _stream: Option<Box<dyn std::any::Any>>,
}

impl Engine {
    pub fn start(repaint: Box<dyn Fn() + Send>) -> Engine {
        let (tx, rx) = channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let (rate, device, stream, cons) = open_output();
        let latency = rate as usize / 25; // 40 ms
        let rb = HeapRb::<[f32; 2]>::new(latency * 2);
        let (prod, cons_local) = rb.split();
        // the device takes its frames from `cons`; without one, a clock does
        let stream = match (stream, cons) {
            (Some(s), Some(slot)) => {
                *slot.lock().expect("slot") = Some(cons_local);
                Some(s)
            }
            _ => {
                spawn_clock(cons_local, rate);
                None
            }
        };
        let st = status.clone();
        std::thread::Builder::new()
            .name("ffx-music-engine".into())
            .spawn(move || engine(rx, prod, st, rate, latency, repaint))
            .expect("engine thread");
        Engine { tx, status, device, _stream: stream }
    }

    pub fn send(&self, c: Cmd) {
        let _ = self.tx.send(c);
    }

    /// A sender for other threads (the MIDI input).
    pub fn sender(&self) -> Sender<Cmd> {
        self.tx.clone()
    }

    pub fn status(&self) -> Status {
        self.status.lock().map(|s| s.clone()).unwrap_or_default()
    }
}

type Prod = ringbuf::HeapProd<[f32; 2]>;
type Cons = ringbuf::HeapCons<[f32; 2]>;
type Slot = Arc<Mutex<Option<Cons>>>;

fn spawn_clock(mut cons: Cons, rate: u32) {
    std::thread::spawn(move || {
        let t0 = std::time::Instant::now();
        let mut taken = 0u64;
        let mut buf = vec![[0f32; 2]; 4096];
        loop {
            std::thread::sleep(Duration::from_millis(10));
            let due = (t0.elapsed().as_secs_f64() * f64::from(rate)) as u64;
            while taken < due {
                let n = ((due - taken) as usize).min(buf.len());
                let got = cons.pop_slice(&mut buf[..n]);
                // silence counts as time too
                taken += n.max(got) as u64;
            }
        }
    });
}

#[cfg(any(windows, feature = "alsa"))]
fn open_output() -> (u32, String, Option<Box<dyn std::any::Any>>, Option<Slot>) {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    let fail = |why: String| (48_000, why, None, None);
    let host = cpal::default_host();
    let Some(device) = host.default_output_device() else { return fail("no sound device".into()) };
    let name = device.description().map(|d| d.to_string()).unwrap_or_else(|_| "sound device".into());
    let Ok(cfg) = device.default_output_config() else { return fail(format!("{name}: no output format")) };
    let rate = cfg.sample_rate();
    let channels = usize::from(cfg.channels());
    let mut config: cpal::StreamConfig = cfg.config();
    config.buffer_size = cpal::BufferSize::Default;
    let slot: Slot = Arc::new(Mutex::new(None));
    let s2 = slot.clone();
    let mut tmp: Vec<[f32; 2]> = Vec::new();
    let mut fill = move |out: &mut [f32]| {
        let frames = out.len() / channels.max(1);
        tmp.resize(frames, [0.0; 2]);
        let got = match s2.try_lock() {
            Ok(mut g) => g.as_mut().map_or(0, |c| c.pop_slice(&mut tmp[..frames])),
            Err(_) => 0,
        };
        for (i, f) in out.chunks_mut(channels.max(1)).enumerate() {
            let s = if i < got { tmp[i] } else { [0.0; 2] };
            for (c, x) in f.iter_mut().enumerate() {
                *x = if c % 2 == 0 { s[0] } else { s[1] };
            }
        }
    };
    let err = |e| eprintln!("sound: {e}");
    let stream = match cfg.sample_format() {
        cpal::SampleFormat::F32 => device.build_output_stream(config, move |d: &mut [f32], _: &_| fill(d), err, None),
        cpal::SampleFormat::I16 => {
            let mut f = Vec::new();
            device.build_output_stream(
                config,
                move |d: &mut [i16], _: &_| {
                    f.resize(d.len(), 0.0);
                    fill(&mut f);
                    for (o, x) in d.iter_mut().zip(&f) {
                        *o = (x * 32767.0) as i16;
                    }
                },
                err,
                None,
            )
        }
        other => return fail(format!("{name}: sample format {other} not supported")),
    };
    match stream {
        Ok(s) => {
            if let Err(e) = s.play() {
                return fail(format!("{name}: {e}"));
            }
            (rate, format!("{name}, {rate} Hz"), Some(Box::new(s)), Some(slot))
        }
        Err(e) => fail(format!("{name}: {e}")),
    }
}

#[cfg(not(any(windows, feature = "alsa")))]
fn open_output() -> (u32, String, Option<Box<dyn std::any::Any>>, Option<Slot>) {
    (48_000, "no sound output (built without the `alsa` feature)".into(), None, None)
}

fn add(into: &mut [[f32; 2]], from: &[[f32; 2]]) {
    for (a, b) in into.iter_mut().zip(from) {
        a[0] += b[0];
        a[1] += b[1];
    }
}

fn merge_max(into: &mut Vec<f32>, from: &[f32]) {
    if into.len() < from.len() {
        into.resize(from.len(), 0.0);
    }
    for (a, b) in into.iter_mut().zip(from) {
        *a = a.max(*b);
    }
}

fn engine(rx: Receiver<Cmd>, mut prod: Prod, status: Arc<Mutex<Status>>, rate: u32, latency: usize, repaint: Box<dyn Fn() + Send>) {
    let mut player: Option<Player> = None;
    // a stopped song's notes fading out
    let mut tail: Option<Player> = None;
    let mut live: Option<Live> = None;
    let mut channels = 0usize;
    let mut tables: Option<Arc<Tables>> = None;
    let mut playing = false;
    let mut metronome = false;
    let mut output = 1.0f32;
    let mut buf: Vec<[f32; 2]> = Vec::new();
    let mut tmp: Vec<[f32; 2]> = Vec::new();
    let chunk = 256usize;
    let mut last_status = std::time::Instant::now();
    let mut busy_ms = 0u32;
    let mut peak = [0f32; 2];
    let mut levels: Vec<f32> = Vec::new();
    // a preview while the song does not play
    let mut alone: Vec<[f32; 2]> = Vec::new();
    let mut alone_at = 0usize;
    loop {
        let sounding = |player: &Option<Player>, tail: &Option<Player>, live: &Option<Live>, playing: bool, alone_left: bool| {
            playing || player.as_ref().is_some_and(Player::fading) || tail.is_some() || live.as_ref().is_some_and(Live::active) || alone_left
        };
        // commands (wait a little when there is nothing to do)
        let busy = sounding(&player, &tail, &live, playing, alone_at < alone.len()) && prod.occupied_len() < latency;
        let first = if busy {
            match rx.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => None,
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(3)) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Disconnected) => return,
                Err(RecvTimeoutError::Timeout) => None,
            }
        };
        let mut cmds: Vec<Cmd> = first.into_iter().collect();
        cmds.extend(rx.try_iter());
        for c in cmds {
            let t0 = std::time::Instant::now();
            match c {
                Cmd::Load { tab, bgm, wd, settings } => {
                    channels = bgm.tracks.len();
                    let mut p = Player::new(tab.clone(), bgm, wd.clone(), rate);
                    p.set_settings(settings);
                    p.restart();
                    p.metronome = metronome;
                    live = Some(Live::new(tab.clone(), wd, p.ram(), channels, rate));
                    tables = Some(tab);
                    player = Some(p);
                    tail = None;
                    playing = false;
                }
                Cmd::Play => {
                    if let Some(p) = &mut player {
                        p.resume();
                        playing = true;
                    }
                }
                Cmd::Pause => {
                    if let Some(p) = &mut player
                        && playing
                    {
                        p.pause();
                    }
                    playing = false;
                }
                Cmd::Stop => {
                    if let Some(p) = &mut player {
                        if tail.is_some() || p.fading() {
                            tail = None;
                            p.restart();
                        } else if playing {
                            tail = Some(p.stop_with_tail());
                        } else {
                            p.restart();
                        }
                    }
                    playing = false;
                }
                Cmd::SeekTick(t) => {
                    tail = None;
                    if let Some(p) = &mut player {
                        p.seek_tick(t);
                    }
                }
                Cmd::Settings(s) => {
                    if let Some(p) = &mut player {
                        p.set_settings(s);
                    }
                }
                Cmd::Bgm(b) => {
                    if let Some(p) = &mut player {
                        p.set_bgm(b);
                    }
                }
                Cmd::Wd(w) => {
                    if let Some(p) = &mut player {
                        p.set_wd(w.clone());
                        if let Some(tab) = &tables {
                            live = Some(Live::new(tab.clone(), w, p.ram(), channels, rate));
                        }
                    }
                }
                Cmd::Preview(s) => match &mut player {
                    Some(p) if playing => p.play_preview(&s),
                    _ => {
                        let master = player.as_ref().map_or(1.0, |p| p.settings().master);
                        alone = preview_alone(&s, rate, master);
                        alone_at = 0;
                    }
                },
                Cmd::Live(l) => {
                    if let Some(lv) = &mut live {
                        match l {
                            LiveCmd::On { ch, key, vel, m } => lv.note_on(ch, key, vel, m),
                            LiveCmd::Off { ch, key } => lv.note_off(ch, key),
                            LiveCmd::Bend { ch, raw } => lv.bend(ch, raw),
                            LiveCmd::AllOff => lv.all_off(),
                        }
                    }
                }
                Cmd::Output(v) => output = v.clamp(0.0, 4.0),
                Cmd::Metronome(on) => {
                    metronome = on;
                    if let Some(p) = &mut player {
                        p.metronome = on;
                    }
                }
            }
            let ms = t0.elapsed().as_millis() as u32;
            if ms > 5 {
                busy_ms = ms;
            }
        }
        let live_gain = demaster::musicjob::gain() * player.as_ref().map_or(1.0, |p| p.settings().master) / 32768.0;
        while sounding(&player, &tail, &live, playing, alone_at < alone.len()) && prod.occupied_len() < latency {
            buf.clear();
            buf.resize(chunk, [0.0; 2]);
            if let Some(p) = &mut player
                && (playing || p.fading())
            {
                tmp.clear();
                p.render(chunk, &mut tmp);
                add(&mut buf, &tmp);
            }
            if let Some(t) = &mut tail {
                tmp.clear();
                t.render(chunk, &mut tmp);
                add(&mut buf, &tmp);
                merge_max(&mut levels, &t.take_levels());
                if !t.fading() {
                    tail = None;
                }
            }
            if let Some(lv) = &mut live
                && lv.active()
            {
                lv.mix(&mut buf, live_gain);
            }
            if alone_at < alone.len() {
                let n = (alone.len() - alone_at).min(chunk);
                add(&mut buf[..n], &alone[alone_at..alone_at + n]);
                alone_at += n;
            }
            for s in &mut buf {
                s[0] = (s[0] * output).clamp(-1.0, 1.0);
                s[1] = (s[1] * output).clamp(-1.0, 1.0);
                peak[0] = peak[0].max(s[0].abs());
                peak[1] = peak[1].max(s[1].abs());
            }
            let mut done = 0;
            while done < buf.len() {
                let n = prod.push_slice(&buf[done..]);
                done += n;
                if n == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            }
        }
        if last_status.elapsed() >= Duration::from_millis(30) {
            last_status = std::time::Instant::now();
            if let Some(p) = &mut player {
                merge_max(&mut levels, &p.take_levels());
            }
            if let Some(lv) = &mut live {
                merge_max(&mut levels, &lv.take_levels());
            }
            let fading = player.as_ref().is_some_and(Player::fading) || tail.is_some();
            if let Ok(mut s) = status.lock() {
                match &player {
                    Some(p) => {
                        let song = p.song();
                        s.loaded = true;
                        s.playing = playing;
                        s.fading = fading;
                        s.finished = p.finished();
                        s.frame = song.frame();
                        s.tick = song.tick;
                        s.bpm = song.bpm();
                        s.voices = song.voices_per_track();
                        s.programs = song.programs();
                        s.active_voices = p.spu().active_voices();
                        s.loops = song.marks.loop_jumps.len();
                        s.levels = std::mem::take(&mut levels);
                        s.levels.resize(channels, 0.0);
                    }
                    None => *s = Status::default(),
                }
                s.peak = peak;
                s.busy_ms = busy_ms;
            }
            levels.clear();
            peak = [0.0; 2];
            if playing || fading || live.as_ref().is_some_and(Live::active) {
                repaint();
            }
            if let Some(p) = &player
                && playing
                && p.finished()
            {
                playing = false;
                repaint();
            }
        }
    }
}

/// A preview (chip level, 48 kHz) at output level and rate, for playing it
/// without the song (nearest-sample rate change).
fn preview_alone(s: &[[i16; 2]], rate: u32, master: f32) -> Vec<[f32; 2]> {
    let g = demaster::musicjob::gain() * master / 32768.0;
    let n = s.len() * rate as usize / 48_000;
    (0..n)
        .map(|i| {
            let j = (i * s.len() / n.max(1)).min(s.len().saturating_sub(1));
            [(f32::from(s[j][0]) * g).clamp(-1.0, 1.0), (f32::from(s[j][1]) * g).clamp(-1.0, 1.0)]
        })
        .collect()
}
