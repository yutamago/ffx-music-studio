//! `--survey`: what every track is made of (channels with notes, notes,
//! instruments, the longest sample a note plays), to tell sequences of many
//! short notes from those that start one long recording.

use anyhow::Result;
use demaster::musicjob::KEEP_PC;
use demaster::sqseq::{Tables, Wd};

use crate::library::Library;
use crate::seq::Seq;

/// One sample a note starts: its length at the pitch it is played.
#[derive(Debug, Clone, Copy)]
pub struct SampleUse {
    pub channel: usize,
    pub inst: usize,
    pub key: u8,
    /// Length of the sample data up to its end block (s, at the played pitch).
    pub seconds: f64,
    /// The rate it plays at (Hz).
    pub rate: f64,
    pub loops: bool,
}

/// ADPCM blocks from `start` up to the one with the end flag; whether it
/// repeats.
pub fn sample_blocks(ram: &[u8], start: u32) -> (usize, bool) {
    let mut p = start as usize & !15;
    let mut n = 0;
    while p + 16 <= ram.len() {
        n += 1;
        let f = ram[p + 1];
        if f & 1 != 0 {
            return (n, f & 2 != 0);
        }
        p += 16;
    }
    (n, false)
}

/// The region a key plays (key ranges only; velocity layers ignored).
fn region_for(wd: &Wd, inst: usize, key: u8) -> Option<usize> {
    let first = *wd.instruments.get(inst)?;
    let regs = wd.instrument_regions(inst);
    let k = regs.iter().position(|r| r.key_hi >= key as i8).unwrap_or(regs.len().saturating_sub(1));
    (!regs.is_empty()).then_some(first + k)
}

/// The samples the notes of `seq` play, longest first (one per instrument
/// and region).
pub fn sample_uses(seq: &Seq, wd: &Wd, tab: &Tables) -> Vec<SampleUse> {
    let mut v: Vec<SampleUse> = Vec::new();
    for c in 0..seq.tracks.len() {
        let notes = seq.notes(c);
        let progs: Vec<(usize, u32, usize)> =
            seq.tracks[c].events.iter().enumerate().filter(|(_, e)| e.op == 0x20).map(|(i, e)| (i, e.tick, usize::from(e.args[0]))).collect();
        for n in &notes {
            let Some(&(_, _, inst)) = progs.iter().rev().find(|p| p.0 < n.on) else { continue };
            let Some(r) = region_for(wd, inst, n.key) else { continue };
            if v.iter().any(|u| u.inst == inst && region_for(wd, u.inst, u.key) == Some(r)) {
                continue;
            }
            let reg = wd.regions[r];
            let (blocks, loops) = sample_blocks(&wd.samples, reg.sample);
            let pitch = f64::from(tab.key_pitch(n.key, reg.tune).max(1));
            let rate = 48_000.0 * pitch / 4096.0;
            v.push(SampleUse { channel: c, inst, key: n.key, seconds: (blocks * 28) as f64 / rate, rate, loops });
        }
    }
    v.sort_by(|a, b| b.seconds.total_cmp(&a.seconds));
    v
}

/// `--survey <track>`: every note of one track with the sample it plays.
pub fn notes(lib: &mut Library, q: &str) -> Result<()> {
    let t = lib.find(q).ok_or_else(|| anyhow::anyhow!("no track {q}"))?.clone();
    let (b, wd, _) = lib.song(t.id)?;
    let seq = Seq::decode(&b)?;
    println!("{} ({:.1} s, loop {:?}..{})", t.label(), seq.seconds_at(seq.end_tick()), seq.loop_start(), seq.loop_end());
    for (i, r) in wd.regions.iter().enumerate() {
        let (blocks, loops) = sample_blocks(&wd.samples, r.sample);
        println!("  region {i}: key_hi {} vel_hi {} sample {:#x} {} blocks ({} samples){}", r.key_hi, r.vel_hi, r.sample, blocks, blocks * 28, if loops { " loops" } else { "" });
    }
    for c in 0..seq.tracks.len() {
        for n in seq.notes(c) {
            println!(
                "  ch{c} tick {} ({:.1} s) len {} ({:.1} s) {} vel {}",
                n.start,
                seq.seconds_at(n.start),
                n.len,
                seq.seconds_at(n.end()) - seq.seconds_at(n.start),
                crate::roll::key_name(n.key),
                n.vel
            );
        }
    }
    Ok(())
}

pub fn run(lib: &mut Library) -> Result<()> {
    println!("id\tname\tchannels_with_notes/channels\tnotes\tseconds\tbank\tlongest samples (ch inst key: s @ kHz, loop)");
    let tracks = lib.tracks.clone();
    for t in &tracks {
        let (b, wd, missing) = lib.song(t.id)?;
        let seq = Seq::decode(&b)?;
        let counts: Vec<usize> = (0..seq.tracks.len()).map(|c| seq.notes(c).len()).collect();
        let with = counts.iter().filter(|&&n| n > 0).count();
        let total: usize = counts.iter().sum();
        let uses = sample_uses(&seq, &wd, &lib.tables);
        let longest: Vec<String> = uses
            .iter()
            .take(3)
            .map(|u| format!("ch{} #{} {}: {:.1} s @ {:.1} kHz{}", u.channel, u.inst, crate::roll::key_name(u.key), u.seconds, u.rate / 1000.0, if u.loops { " loop" } else { "" }))
            .collect();
        let bank = if missing.is_some() {
            format!("WD {} MISSING", wd.id)
        } else {
            format!("WD {} ({} inst)", wd.id, wd.instruments.len())
        };
        let streamed = if KEEP_PC.contains(&t.pc_name.as_str()) { " STREAMED-PARTS" } else { "" };
        println!(
            "{}\t{}{}\t{}/{}\t{}\t{:.0}\t{}\t{}",
            t.id,
            t.label(),
            streamed,
            with,
            seq.tracks.len(),
            total,
            seq.seconds_at(seq.end_tick()),
            bank,
            longest.join("; ")
        );
    }
    Ok(())
}
