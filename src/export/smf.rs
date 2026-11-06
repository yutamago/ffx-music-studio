//! A sequence as a Standard MIDI File (type 1), from what the PS2 driver
//! itself does: the song runs through `sqseq::Song` with its event log on
//! (`Song::log`), so ties, key-offs, counted loops and the loop point come
//! out as the driver plays them, on its tick timeline (48 ticks a quarter
//! note; MIDI tempo = the driver's BPM).

use std::collections::BTreeMap;
use std::sync::Arc;

use demaster::spu2::Spu2;
use demaster::sqseq::{Bgm, LogEntry, SeqEvent, Song, Tables, Wd};

pub const PPQN: u16 = 48;
/// MIDI channels for the note tracks: all but 10 (index 9, General MIDI
/// drums in most synths).
const CHANNELS: [u8; 15] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 10, 11, 12, 13, 14, 15];

/// What the export learned about a song.
#[derive(Debug, Clone, Default)]
pub struct SongInfo {
    pub ticks: u64,
    pub loop_start: Option<u64>,
    pub loop_end: Option<u64>,
    pub tempos: Vec<(u64, u8)>,
    pub depths: Vec<(u64, i8)>,
    pub repeats: usize,
    pub notes: usize,
    /// The largest difference (ms) between a note's time in the MIDI file
    /// (ticks through the tempo map) and the interrupt the driver played
    /// its tick in.
    pub timing_dev_ms: f64,
    /// Key-ons the driver dropped for want of a voice (not in the file).
    pub dropped: usize,
    /// Per track: notes, MIDI port, programs used.
    pub tracks: Vec<TrackInfo>,
}

#[derive(Debug, Clone, Default)]
pub struct TrackInfo {
    pub notes: usize,
    pub port: u8,
    pub programs: Vec<usize>,
}

/// Runs the driver with its log on, to the first jump back to the loop
/// point, the song's end or `max_ticks`.
pub fn run_log(tab: Arc<Tables>, bgm: Arc<Bgm>, wd: Arc<Wd>, max_ticks: u64) -> Vec<LogEntry> {
    let mut spu = Spu2::new(wd.samples.clone());
    let mut song = Song::new(tab, bgm, wd, &mut spu);
    song.log = Some(Vec::new());
    let mut guard = 0u64;
    loop {
        song.interrupt_skip(&mut spu);
        guard += 1;
        let log = song.log.as_ref().expect("log");
        let jumped = log.iter().rev().take(64).any(|e| e.event == SeqEvent::LoopJump);
        if jumped || !song.playing() || song.ticks_run() > max_ticks || guard > 240 * 3600 {
            break;
        }
    }
    let mut log = song.log.take().expect("log");
    if let Some(j) = log.iter().position(|e| e.event == SeqEvent::LoopJump) {
        log.truncate(j + 1);
    }
    log
}

fn varlen(out: &mut Vec<u8>, mut v: u32) {
    let mut b = [0u8; 5];
    let mut n = 0;
    loop {
        b[n] = (v & 0x7f) as u8;
        n += 1;
        v >>= 7;
        if v == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        out.push(b[i] | if i > 0 { 0x80 } else { 0 });
    }
}

/// One MIDI event at a tick; `order` sorts events of one tick (offs first,
/// then controllers and programs, then ons).
#[derive(Debug, Clone)]
struct Ev {
    tick: u64,
    order: u8,
    seq: usize,
    bytes: Vec<u8>,
}

struct Track {
    evs: Vec<Ev>,
}

impl Track {
    fn push(&mut self, tick: u64, order: u8, bytes: Vec<u8>) {
        let seq = self.evs.len();
        self.evs.push(Ev { tick, order, seq, bytes });
    }
    fn meta(&mut self, tick: u64, kind: u8, data: &[u8]) {
        let mut b = vec![0xff, kind];
        varlen(&mut b, data.len() as u32);
        b.extend_from_slice(data);
        self.push(tick, 1, b);
    }
    fn encode(mut self, end: u64) -> Vec<u8> {
        self.evs.sort_by_key(|e| (e.tick, e.order, e.seq));
        let mut d = Vec::new();
        let mut t = 0u64;
        for e in &self.evs {
            varlen(&mut d, (e.tick - t) as u32);
            t = e.tick;
            d.extend_from_slice(&e.bytes);
        }
        varlen(&mut d, end.saturating_sub(t) as u32);
        d.extend_from_slice(&[0xff, 0x2f, 0]);
        let mut c = b"MTrk".to_vec();
        c.extend_from_slice(&(d.len() as u32).to_be_bytes());
        c.extend_from_slice(&d);
        c
    }
}

/// Seconds of `tick` through a tempo map (the driver starts at 1 BPM).
pub fn tick_seconds(tick: u64, tempos: &[(u64, u8)]) -> f64 {
    let mut s = 0.0;
    let (mut at, mut bpm) = (0u64, 1.0f64);
    for &(t, b) in tempos {
        if t >= tick {
            break;
        }
        s += (t - at) as f64 * 60.0 / (bpm * f64::from(PPQN));
        at = t;
        bpm = f64::from(b.max(1));
    }
    s + (tick - at) as f64 * 60.0 / (bpm * f64::from(PPQN))
}

fn cc(ch: u8, n: u8, v: u8) -> Vec<u8> {
    vec![0xb0 | ch, n, v.min(127)]
}

/// The song's volume factor (`(sv + 1) / 512` of the driver's volume).
fn song_gain(bgm: &Bgm) -> f64 {
    let sv = (i32::from(bgm.volume & 0x7f) + 1) * 0x1ff >> 7;
    f64::from(sv + 1) / 512.0
}

/// The SMF bytes and what was found. `title` names the file's first track,
/// `prog_name` the instruments (for the track names); `bank` is the SF2
/// bank number of the sequence's WD bank.
pub fn build(bgm: &Bgm, bank: u16, log: &[LogEntry], title: &str, prog_name: &dyn Fn(usize) -> String) -> (Vec<u8>, SongInfo) {
    let n = bgm.tracks.len();
    let mut info = SongInfo::default();
    let end_tick = log.last().map_or(0, |e| e.tick);
    info.ticks = end_tick;
    let mut conductor = Track { evs: Vec::new() };
    conductor.meta(0, 0x03, title.as_bytes());
    conductor.meta(0, 0x58, &[4, 2, 24, 8]);
    // the driver starts at 1 BPM until the first tempo command
    if !log.iter().any(|e| e.tick == 0 && matches!(e.event, SeqEvent::Tempo(_))) {
        conductor.meta(0, 0x51, &60_000_000u32.to_be_bytes()[1..]);
    }
    // per track: does it play notes?
    let mut has_notes = vec![false; n];
    for e in log {
        if let SeqEvent::NoteOn { .. } = e.event
            && e.track < n
        {
            has_notes[e.track] = true;
        }
    }
    let mut chans = vec![(0u8, 0u8); n];
    let mut k = 0usize;
    for (t, c) in chans.iter_mut().enumerate() {
        if has_notes[t] {
            *c = (CHANNELS[k % CHANNELS.len()], (k / CHANNELS.len()) as u8);
            k += 1;
        }
    }
    let gain = song_gain(bgm);
    let (msb, lsb) = if bank < 128 { (bank as u8, 0u8) } else { ((bank >> 7) as u8, (bank & 0x7f) as u8) };
    let mut tracks: Vec<Track> = (0..n).map(|_| Track { evs: Vec::new() }).collect();
    let mut tinfo: Vec<TrackInfo> = (0..n).map(|t| TrackInfo { port: chans[t].1, ..TrackInfo::default() }).collect();
    // sounding notes per track: key -> (start, vel)
    let mut sounding: Vec<BTreeMap<u8, (u64, u8)>> = vec![BTreeMap::new(); n];
    let mut tie = vec![false; n];
    let mut has_prog = vec![false; n];
    // finished notes per track: (start, end, key, vel)
    let mut notes: Vec<Vec<(u64, u64, u8, u8)>> = vec![Vec::new(); n];
    for (t, tr) in tracks.iter_mut().enumerate() {
        if !has_notes[t] {
            continue;
        }
        let ch = chans[t].0;
        if chans[t].1 > 0 {
            tr.meta(0, 0x21, &[chans[t].1]);
        }
        tr.meta(0, 0x20, &[ch]);
        // the driver's defaults: bend range 2, full volume and expression,
        // centre, no reverb send
        for b in [cc(ch, 101, 0), cc(ch, 100, 0), cc(ch, 6, 2), cc(ch, 38, 0), cc(ch, 101, 127), cc(ch, 100, 127)] {
            tr.push(0, 0, b);
        }
        tr.push(0, 0, cc(ch, 7, (127.0 * gain).round() as u8));
        tr.push(0, 0, cc(ch, 11, 127));
        tr.push(0, 0, cc(ch, 10, 64));
        tr.push(0, 0, cc(ch, 91, 0));
    }
    let end_note = |notes: &mut Vec<(u64, u64, u8, u8)>, s: &mut BTreeMap<u8, (u64, u8)>, key: u8, at: u64| {
        if let Some((st, v)) = s.remove(&key)
            && at > st
        {
            notes.push((st, at, key, v));
        }
    };
    for e in log {
        let (t, at) = (e.track, e.tick);
        match e.event {
            SeqEvent::Tempo(b) if b > 0 => {
                conductor.meta(at, 0x51, &(60_000_000u32 / u32::from(b)).to_be_bytes()[1..]);
                info.tempos.push((at, b));
            }
            SeqEvent::Depth(d) => info.depths.push((at, d)),
            SeqEvent::LoopPoint => {
                info.loop_start = Some(at);
                conductor.meta(at, 0x06, b"loopStart");
                for (ti, tr) in tracks.iter_mut().enumerate() {
                    if has_notes[ti] {
                        tr.push(at, 0, cc(chans[ti].0, 111, 0));
                    }
                }
            }
            SeqEvent::LoopJump => {
                info.loop_end = Some(at);
                conductor.meta(at, 0x06, b"loopEnd");
            }
            SeqEvent::RepeatStart => {
                info.repeats += 1;
                conductor.meta(at, 0x06, format!("repeat start (ch{t:02})").as_bytes());
            }
            SeqEvent::RepeatEnd { pass, count } => {
                conductor.meta(at, 0x06, format!("repeat {pass}/{count} (ch{t:02})").as_bytes());
            }
            _ => {}
        }
        if t >= n || !has_notes[t] {
            continue;
        }
        let ch = chans[t].0;
        let tr = &mut tracks[t];
        match e.event {
            SeqEvent::NoteOn { key, vel } => {
                if !has_prog[t] || key > 127 {
                    continue;
                }
                let midi_s = tick_seconds(at, &info.tempos);
                let drv_s = e.frame as f64 / 48000.0;
                info.timing_dev_ms = info.timing_dev_ms.max((midi_s - drv_s).abs() * 1000.0);
                // velocity 0 is full volume in the driver
                let vel = if vel == 0 || vel > 127 { 127 } else { vel };
                if tie[t] && !sounding[t].is_empty() {
                    // tied: the voice moves on to the new key
                    if sounding[t].contains_key(&key) {
                        continue;
                    }
                    let keys: Vec<u8> = sounding[t].keys().copied().collect();
                    for k in keys {
                        end_note(&mut notes[t], &mut sounding[t], k, at);
                    }
                }
                // the same key again: the old note ends with it
                end_note(&mut notes[t], &mut sounding[t], key, at);
                sounding[t].insert(key, (at, vel));
            }
            SeqEvent::NoteOff { key } => {
                // a key-off in the same tick as its key-on: never sounds
                if let Some(&(st, _)) = sounding[t].get(&key)
                    && st == at
                {
                    sounding[t].remove(&key);
                    continue;
                }
                end_note(&mut notes[t], &mut sounding[t], key, at);
            }
            SeqEvent::Dropped { key } => {
                // no voice was free: the PS2 never played it
                if let Some(&(st, _)) = sounding[t].get(&key)
                    && st == at
                {
                    sounding[t].remove(&key);
                    info.dropped += 1;
                }
            }
            SeqEvent::Tie(on) => tie[t] = on,
            SeqEvent::Program(p) => {
                has_prog[t] = true;
                tr.push(at, 2, cc(ch, 0, msb));
                tr.push(at, 2, cc(ch, 32, lsb));
                tr.push(at, 2, vec![0xc0 | ch, (p & 0x7f) as u8]);
                if !tinfo[t].programs.contains(&p) {
                    tinfo[t].programs.push(p);
                }
            }
            SeqEvent::Pedal(on) => tr.push(at, 2, cc(ch, 64, if on { 127 } else { 0 })),
            SeqEvent::Bend(raw) => {
                let v = (i32::from(raw) + 0x2000).clamp(0, 0x3fff) as u16;
                tr.push(at, 2, vec![0xe0 | ch, (v & 0x7f) as u8, (v >> 7) as u8]);
            }
            SeqEvent::BendRange(r) => {
                for b in [cc(ch, 101, 0), cc(ch, 100, 0), cc(ch, 6, r.unsigned_abs()), cc(ch, 38, 0), cc(ch, 101, 127), cc(ch, 100, 127)] {
                    tr.push(at, 2, b);
                }
            }
            SeqEvent::Volume(v) => tr.push(at, 2, cc(ch, 7, (127.0 * f64::from(v) / 16383.0 * gain).round() as u8)),
            SeqEvent::Expression(v) => tr.push(at, 2, cc(ch, 11, (127.0 * f64::from(v) / 16383.0).round() as u8)),
            SeqEvent::Pan(p) => tr.push(at, 2, cc(ch, 10, (p - 0x40).clamp(0, 127) as u8)),
            SeqEvent::Wet(on) => tr.push(at, 2, cc(ch, 91, if on { 127 } else { 0 })),
            SeqEvent::End => {
                let keys: Vec<u8> = sounding[t].keys().copied().collect();
                for k in keys {
                    end_note(&mut notes[t], &mut sounding[t], k, at);
                }
            }
            _ => {}
        }
    }
    // notes still sounding end with the file (the loop end)
    let fin = end_tick.max(1);
    for t in 0..n {
        let keys: Vec<u8> = sounding[t].keys().copied().collect();
        for k in keys {
            if let Some((st, v)) = sounding[t].remove(&k) {
                notes[t].push((st, fin.max(st + 1), k, v));
            }
        }
    }
    let mut file_end = fin;
    for t in 0..n {
        let ch = chans[t].0;
        for &(st, en, key, vel) in &notes[t] {
            tracks[t].push(st, 3, vec![0x90 | ch, key, vel]);
            tracks[t].push(en, 0, vec![0x80 | ch, key, 0]);
            file_end = file_end.max(en);
        }
        tinfo[t].notes = notes[t].len();
        info.notes += notes[t].len();
    }
    for (t, tr) in tracks.iter_mut().enumerate() {
        let name = match tinfo[t].programs.first() {
            Some(&p) => format!("ch{t:02} {}", prog_name(p)),
            None => format!("ch{t:02}"),
        };
        tr.meta(0, 0x03, name.as_bytes());
    }
    info.tracks = tinfo;
    let mut out = b"MThd".to_vec();
    out.extend_from_slice(&6u32.to_be_bytes());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&((n + 1) as u16).to_be_bytes());
    out.extend_from_slice(&PPQN.to_be_bytes());
    out.extend_from_slice(&conductor.encode(file_end));
    for tr in tracks {
        out.extend_from_slice(&tr.encode(file_end));
    }
    (out, info)
}
