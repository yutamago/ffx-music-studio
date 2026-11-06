//! A Standard MIDI File played the way a DAW would: its tempo map turns
//! ticks into samples, and blocks of `block` samples go to
//! [`Engine::process`] with the transport (position, tempo) at each
//! block's start and the block's events. For the tests and
//! `examples/render_midi.rs`.

use anyhow::{Context, Result, bail, ensure};

use crate::engine::{Engine, Event, Transport};
use crate::midi::Midi;

/// A MIDI file's channel events (absolute ticks, file order) and tempo map.
#[derive(Debug, Clone, Default)]
pub struct Smf {
    pub ppqn: u16,
    pub events: Vec<(u64, Midi)>,
    /// (tick, microseconds a quarter note)
    pub tempos: Vec<(u64, u32)>,
    pub end_tick: u64,
}

fn varlen(d: &[u8], p: &mut usize) -> Result<u32> {
    let mut v = 0u32;
    for _ in 0..4 {
        let b = *d.get(*p).context("SMF: truncated")?;
        *p += 1;
        v = v << 7 | u32::from(b & 0x7f);
        if b & 0x80 == 0 {
            return Ok(v);
        }
    }
    bail!("SMF: bad variable-length number")
}

impl Smf {
    pub fn parse(b: &[u8]) -> Result<Smf> {
        ensure!(b.get(..4) == Some(b"MThd"), "not a MIDI file");
        let ntracks = u16::from_be_bytes([b[10], b[11]]);
        let ppqn = u16::from_be_bytes([b[12], b[13]]);
        ensure!(ppqn & 0x8000 == 0, "SMPTE time is not supported");
        let mut p = 8 + u32::from_be_bytes(b[4..8].try_into()?) as usize;
        let mut all: Vec<(u64, usize, usize, Midi)> = Vec::new();
        let mut smf = Smf { ppqn, ..Smf::default() };
        for ti in 0..usize::from(ntracks) {
            ensure!(b.get(p..p + 4) == Some(b"MTrk"), "SMF: track {ti} missing");
            let len = u32::from_be_bytes(b[p + 4..p + 8].try_into()?) as usize;
            let d = b.get(p + 8..p + 8 + len).context("SMF: track truncated")?;
            p += 8 + len;
            let (mut q, mut tick, mut status) = (0usize, 0u64, 0u8);
            let mut n = 0usize;
            // MIDI port (meta 0x21): the plugin's part
            let mut port = 0u8;
            while q < d.len() {
                tick += u64::from(varlen(d, &mut q)?);
                let mut s = d[q];
                if s & 0x80 != 0 {
                    q += 1;
                    if s < 0xf0 {
                        status = s;
                    }
                } else {
                    s = status;
                }
                match s {
                    0xff => {
                        let kind = d[q];
                        q += 1;
                        let l = varlen(d, &mut q)? as usize;
                        if kind == 0x21 && l == 1 {
                            port = d[q];
                        }
                        if kind == 0x51 && l == 3 {
                            smf.tempos.push((tick, u32::from(d[q]) << 16 | u32::from(d[q + 1]) << 8 | u32::from(d[q + 2])));
                        }
                        q += l;
                    }
                    0xf0 | 0xf7 => {
                        let l = varlen(d, &mut q)? as usize;
                        q += l;
                    }
                    _ => {
                        let nd = if matches!(s & 0xf0, 0xc0 | 0xd0) { 1 } else { 2 };
                        let mut m = vec![s];
                        m.extend_from_slice(d.get(q..q + nd).context("SMF: truncated event")?);
                        q += nd;
                        if let Some(e) = Midi::parse(&m) {
                            all.push((tick, ti, n, e.on_part(port)));
                            n += 1;
                        }
                    }
                }
                smf.end_tick = smf.end_tick.max(tick);
            }
        }
        all.sort_by_key(|e| (e.0, e.1, e.2));
        smf.events = all.into_iter().map(|e| (e.0, e.3)).collect();
        smf.tempos.sort_by_key(|t| t.0);
        Ok(smf)
    }

    /// (tick, seconds, µs a quarter) at each tempo change, from tick 0.
    fn segments(&self) -> Vec<(u64, f64, u32)> {
        let mut v = vec![(0u64, 0.0f64, 500_000u32)];
        for &(t, us) in &self.tempos {
            let &(t0, s0, u0) = v.last().expect("segment");
            let s = s0 + (t - t0) as f64 * f64::from(u0) / 1e6 / f64::from(self.ppqn);
            if t == t0 {
                v.pop();
            }
            v.push((t, s, us));
        }
        v
    }

    pub fn tick_seconds(&self, tick: f64) -> f64 {
        let seg = self.segments();
        let &(t0, s0, us) = seg.iter().rev().find(|s| s.0 as f64 <= tick).unwrap_or(&seg[0]);
        s0 + (tick - t0 as f64) * f64::from(us) / 1e6 / f64::from(self.ppqn)
    }

    /// Position (quarter notes) and tempo (BPM) at `sec`.
    pub fn position(&self, sec: f64) -> (f64, f64) {
        let seg = self.segments();
        let &(t0, s0, us) = seg.iter().rev().find(|s| s.1 <= sec).unwrap_or(&seg[0]);
        let ticks = t0 as f64 + (sec - s0) * 1e6 / f64::from(us) * f64::from(self.ppqn);
        (ticks / f64::from(self.ppqn), 60e6 / f64::from(us))
    }
}

/// A DAW playing a MIDI file from its start, block by block.
pub struct Host<'a> {
    smf: &'a Smf,
    rate: f64,
    timed: Vec<(u64, Midi)>,
    at: usize,
    pos: u64,
}

impl<'a> Host<'a> {
    pub fn new(smf: &'a Smf, rate: f64) -> Self {
        let mut timed: Vec<(u64, Midi)> =
            smf.events.iter().map(|&(t, m)| ((smf.tick_seconds(t as f64) * rate).round() as u64, m)).collect();
        timed.sort_by_key(|e| e.0);
        Host { smf, rate, timed, at: 0, pos: 0 }
    }

    /// The next `n` samples.
    pub fn block(&mut self, engine: &mut Engine, n: usize, out: &mut Vec<[f32; 2]>) {
        let (mut l, mut r) = (vec![0f32; n], vec![0f32; n]);
        let (ppq, tempo) = self.smf.position(self.pos as f64 / self.rate);
        let mut evs = Vec::new();
        while self.at < self.timed.len() && self.timed[self.at].0 < self.pos + n as u64 {
            evs.push(Event { at: (self.timed[self.at].0.saturating_sub(self.pos)) as u32, msg: self.timed[self.at].1 });
            self.at += 1;
        }
        let tr = Transport { playing: true, ppq: Some(ppq), tempo: Some(tempo) };
        engine.process(&tr, &evs, &mut l, &mut r);
        out.extend(l.iter().zip(&r).map(|(a, b)| [*a, *b]));
        self.pos += n as u64;
    }
}

/// Renders `frames` output samples of `smf` through `engine` in blocks of
/// `block`, the transport playing from the file's start.
pub fn render(engine: &mut Engine, smf: &Smf, sample_rate: f64, block: usize, frames: usize) -> Vec<[f32; 2]> {
    let mut host = Host::new(smf, sample_rate);
    let mut out = Vec::with_capacity(frames);
    while out.len() < frames {
        let n = block.min(frames - out.len());
        host.block(engine, n, &mut out);
    }
    out
}
