//! A WD instrument bank as a SoundFont 2: samples decoded from PS-ADPCM
//! with their loops, one SF2 instrument per WD instrument with a zone per
//! key/velocity rectangle of each region (as `Wd::region_for` picks them),
//! root key and tuning from the region's tuning word, the SPU2 ADSR fitted
//! to the SF2 volume envelope (README, "Export": the conversion and its
//! limits), and the presets at bank = WD id, program = instrument number.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use demaster::spu2;
use demaster::sqseq::{Region, Wd};

use super::riff;

/// Samples of padding (zeros) after each sample, as the SF2 spec asks.
const PAD: usize = 46;
/// Samples of the loop start copied after a loop's end (for interpolation).
const GUARD: usize = 8;

/// A decoded sample of a bank.
#[derive(Debug, Clone)]
pub struct Sample {
    pub name: String,
    pub pcm: Vec<i16>,
    /// Loop start and end (end exclusive) in samples, if it loops.
    pub looped: Option<(usize, usize)>,
    /// The rate it plays at for `root` (Hz).
    pub rate: u32,
    pub root: u8,
}

/// One zone of an instrument.
#[derive(Debug, Clone)]
pub struct Zone {
    pub keys: (u8, u8),
    pub vels: (u8, u8),
    pub sample: usize,
    /// Cents to add to the sample's own tuning.
    pub tune_cents: i32,
    /// The key does not change the pitch (WD flag 0x20000).
    pub fixed: bool,
    /// -500 (left) .. 500 (right).
    pub pan: i16,
    /// Centibels.
    pub atten: i16,
    pub env: Env,
    pub reverb: bool,
}

/// The volume envelope in SF2 units (timecents, centibels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Env {
    pub attack: i16,
    pub decay: i16,
    pub sustain: i16,
    pub release: i16,
}

#[derive(Debug, Clone)]
pub struct Instrument {
    pub name: String,
    pub program: u8,
    pub zones: Vec<Zone>,
}

/// A bank ready to write: its samples and instruments.
#[derive(Debug, Clone)]
pub struct Bank {
    /// The SF2 bank number of its presets (the MIDI files' bank select).
    pub number: u16,
    pub samples: Vec<Sample>,
    pub instruments: Vec<Instrument>,
}

/// The tuning word's offset in 1/256 semitones (`(tune as i32) >> 16`).
fn tune_s(r: &Region) -> i32 {
    (r.tune as i32) >> 16
}

/// Cents from the 48 kHz rate at which key `k` plays region `r` (the
/// driver's pitch: `key << 8 + tune >> 16`, 60 << 8 being 0x1000 = 48 kHz).
fn cents_at(r: &Region, k: i32) -> f64 {
    f64::from(k * 256 + tune_s(r) - 60 * 256) * 100.0 / 256.0
}

/// Decodes a sample from the bank's sound RAM as the voice plays it: from
/// `ssa`, the loop start flag (4) moving the loop start, a loop end (1)
/// with repeat (2) jumping to it, without repeat ending it. Long samples in
/// 0x2000-byte pieces (WD flag 0x10) move the loop start on to the next
/// piece as the driver does, so they play through. A jump to a block
/// already decoded is the loop.
pub fn decode(ram: &[u8], ssa: u32, lsa: u32, pieces: bool) -> (Vec<i16>, Option<(usize, usize)>) {
    let mut pcm = Vec::new();
    let mut hist = [0i32; 2];
    let mut seen: BTreeMap<u32, usize> = BTreeMap::new();
    let (mut nax, mut lsa) = (ssa & !15, lsa & !15);
    let cap = 1 << 24;
    loop {
        let a = nax as usize;
        if a + 16 > ram.len() || pcm.len() > cap {
            return (pcm, None);
        }
        if let Some(&i) = seen.get(&nax) {
            let end = pcm.len();
            return (pcm, (end > i).then_some((i, end)));
        }
        seen.insert(nax, pcm.len());
        let b = &ram[a..a + 16];
        let flags = b[1];
        if flags & 4 != 0 {
            lsa = nax;
        }
        pcm.extend_from_slice(&spu2::decode_block(b, &mut hist));
        if pieces && lsa <= nax && nax < lsa + 0x2000 {
            lsa += 0x2000;
        }
        if flags & 1 != 0 {
            if flags & 2 == 0 {
                return (pcm, None);
            }
            nax = lsa;
        } else {
            nax += 16;
        }
    }
}

fn timecents(samples: f64) -> i16 {
    let s = samples / f64::from(spu2::RATE);
    if s < 0.001 { -12000 } else { (1200.0 * s.log2()).round().clamp(-12000.0, 8000.0) as i16 }
}

/// Fits an SF2 volume envelope to the SPU2 envelope of ADSR registers
/// `adsr` (`spu2::envelope_trace`). SF2's decay and release fall linearly
/// in dB, their time given for 100 dB; the fit matches the time the SPU2
/// envelope takes to fall 30 dB (decay: to the sustain level when it holds
/// there).
pub fn fit_env(adsr: u32) -> Env {
    let (a1, a2) = (adsr as u16, (adsr >> 16) as u16);
    const MAX: usize = 10 * spu2::RATE as usize;
    let full = 0x7fff as f64;
    let db = |l: i32| if l <= 0 { -200.0 } else { 20.0 * (f64::from(l) / full).log10() };
    let t = spu2::envelope_trace(a1, a2, usize::MAX, MAX);
    let peak = t.iter().position(|&l| l >= 0x7fff);
    let attack = peak.map_or(-12000, |p| timecents(p as f64));
    let (decay, sustain) = match peak {
        None => (-12000, 0),
        Some(p) => {
            let sl = (((a1 & 0xf) as i32 + 1) * 0x800).min(0x7fff);
            let sl_db = db(sl);
            // where the decay ends, then what the sustain phase does
            let d_end = t[p..].iter().position(|&l| l <= sl).map(|i| p + i);
            let last = *t.last().unwrap_or(&0);
            match d_end {
                Some(e) if db(last) > sl_db - 1.0 || a2 & 0x4000 == 0 => {
                    // holds (or rises: kept at the level reached)
                    let dec = if sl >= 0x7fff { -12000 } else { timecents((e - p) as f64 * 100.0 / -sl_db) };
                    (dec, (-sl_db * 10.0).round() as i16)
                }
                _ => {
                    // falls on: one decay to silence through the same -30 dB point
                    let to = t[p..].iter().position(|&l| db(l) <= -30.0);
                    let dec = match to {
                        Some(i) => timecents(i as f64 * 100.0 / 30.0),
                        None => {
                            let fall = -db(last).max(-99.0);
                            if fall < 0.5 { 8000 } else { timecents((t.len() - p) as f64 * 100.0 / fall) }
                        }
                    };
                    (dec, 1000)
                }
            }
        }
    };
    // release from full level
    let off = peak.unwrap_or(0);
    let r = spu2::envelope_trace(a1, a2, off, off + MAX);
    let rel = r[off.min(r.len())..].iter().position(|&l| db(l) <= -30.0).map_or(8000, |i| timecents(i as f64 * 100.0 / 30.0));
    Env { attack, decay, sustain, release: rel }
}

/// The zones of instrument `inst`: for every key and velocity the region
/// the driver picks (`Wd::region_for`), merged into rectangles.
fn zones_of(wd: &Wd, inst: usize) -> Vec<(usize, (u8, u8), (u8, u8))> {
    let mut out = Vec::new();
    // open rectangles: (region, vlo, vhi) -> first key
    let mut open: BTreeMap<(usize, u8, u8), u8> = BTreeMap::new();
    let mut prev: Vec<(usize, u8, u8)> = Vec::new();
    for key in 0..=128u16 {
        let mut runs: Vec<(usize, u8, u8)> = Vec::new();
        if key < 128 {
            for vel in 1..128u8 {
                let r = wd.region_for(inst, key as u8, vel);
                match runs.last_mut() {
                    Some(l) if l.0 == r => l.2 = vel,
                    _ => runs.push((r, if vel == 1 { 0 } else { vel }, vel)),
                }
            }
        }
        for p in &prev {
            if !runs.contains(p) {
                let lo = open.remove(p).expect("open");
                out.push((p.0, (lo, key as u8 - 1), (p.1, p.2)));
            }
        }
        for r in &runs {
            open.entry(*r).or_insert(key as u8);
        }
        prev = runs;
    }
    out.sort_by_key(|z| (z.1, z.2, z.0));
    out
}

/// Builds the SF2 view of a bank.
pub fn bank(wd: &Wd, number: u16) -> Bank {
    let mut samples: Vec<Sample> = Vec::new();
    let mut by_addr: BTreeMap<(u32, u32, bool), usize> = BTreeMap::new();
    let mut envs: BTreeMap<u32, Env> = BTreeMap::new();
    let mut instruments = Vec::new();
    if wd.regions.is_empty() {
        return Bank { number, samples, instruments };
    }
    for inst in 0..wd.instruments.len().min(128) {
        let mut zones = Vec::new();
        for (ri, keys, vels) in zones_of(wd, inst) {
            let pair = wd.regions[ri].flags & 1 != 0 && ri + 1 < wd.regions.len();
            let parts: Vec<(usize, Option<i16>)> = if pair { vec![(ri, Some(-500)), (ri + 1, Some(500))] } else { vec![(ri, None)] };
            for (r_i, forced_pan) in parts {
                let r = wd.regions[r_i];
                let pieces = r.flags & 0x10 != 0;
                let ssa = r.sample;
                let lsa = if pieces { ssa + 0x2000 } else { ssa + r.loop_off };
                let fixed = r.flags & 0x20000 != 0;
                let si = *by_addr.entry((ssa & !15, lsa & !15, pieces)).or_insert_with(|| {
                    let (pcm, looped) = decode(&wd.samples, ssa, lsa, pieces);
                    // the rate at the zone's home key: C4 (60) if in range
                    let home = if fixed { r.key_hi.max(0) as u8 } else { 60u8.clamp(keys.0, keys.1) };
                    let rate = (48000.0 * 2f64.powf(cents_at(&r, i32::from(home)) / 1200.0)).round().clamp(200.0, 384000.0) as u32;
                    samples.push(Sample {
                        name: format!("w{:03}s{:04}", wd.id, samples.len()),
                        pcm,
                        looped,
                        rate,
                        root: home,
                    });
                    samples.len() - 1
                });
                let s = &samples[si];
                let own = 1200.0 * (f64::from(s.rate) / 48000.0).log2();
                // SF2: cents = own + (key - root) * 100 + tune (fixed: own +
                // tune), the driver: cents_at(key) (fixed: cents_at(key_hi))
                let k = if fixed { i32::from(r.key_hi) } else { i32::from(s.root) };
                let tune_cents = (cents_at(&r, k) - own).round() as i32;
                let rv = i32::from(r.vol & 0x7f);
                let atten = if rv == 0 { 1440 } else { (-200.0 * (f64::from(rv + 1) / 128.0).log10()).round() as i16 };
                let pan = forced_pan.unwrap_or(if r.pan & 0x80 != 0 {
                    let x = i32::from(r.pan & 0x7f) + 0x40;
                    let x = if x == 0 { 0x80 } else { x };
                    (((x - 0x80) as f64 / 64.0 * 500.0).round() as i16).clamp(-500, 500)
                } else {
                    0
                });
                let env = *envs.entry(r.adsr).or_insert_with(|| fit_env(r.adsr));
                zones.push(Zone { keys, vels, sample: si, tune_cents, fixed, pan, atten, env, reverb: r.rev != 0 });
            }
        }
        instruments.push(Instrument { name: format!("w{:03}i{:03}", wd.id, inst), program: inst as u8, zones });
    }
    Bank { number, samples, instruments }
}

/// Writes one sample as a 16-bit mono WAV with its loop and root key in a
/// `smpl` chunk.
pub fn write_wav(s: &Sample, path: &Path) -> Result<()> {
    riff::write_wav_i16(path, s.rate, 1, &s.pcm, Some((s.root, s.looped)))
}

/// A sample laid out for SF2 players, which want 8 points before a loop
/// and a loop of at least 32 points: the loop is repeated (once more after
/// itself, moving it on by its length, or several times over), which plays
/// the same samples.
fn sf2_layout(s: &Sample) -> std::borrow::Cow<'_, Sample> {
    let Some((mut a, mut b)) = s.looped else { return std::borrow::Cow::Borrowed(s) };
    if a >= 8 && b - a >= 32 {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut pcm = s.pcm[..b].to_vec();
    let body = s.pcm[a..b].to_vec();
    while b - a < 32 {
        pcm.extend_from_slice(&body);
        b += body.len();
    }
    while a < 8 {
        let l = b - a;
        let again = pcm[a..b].to_vec();
        pcm.extend_from_slice(&again);
        a += l;
        b += l;
    }
    std::borrow::Cow::Owned(Sample { pcm, looped: Some((a, b)), ..s.clone() })
}

struct Buf(Vec<u8>);
impl Buf {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn i16(&mut self, v: i16) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_le_bytes());
    }
    fn name(&mut self, s: &str) {
        let mut b = [0u8; 20];
        for (i, c) in s.bytes().take(19).enumerate() {
            b[i] = c;
        }
        self.0.extend_from_slice(&b);
    }
}

// generator numbers (SF2 2.01, 8.1.2)
const G_PAN: u16 = 17;
const G_REVERB: u16 = 16;
const G_ATTACK: u16 = 34;
const G_DECAY: u16 = 36;
const G_SUSTAIN: u16 = 37;
const G_RELEASE: u16 = 38;
const G_INSTRUMENT: u16 = 41;
const G_KEYRANGE: u16 = 43;
const G_VELRANGE: u16 = 44;
const G_ATTEN: u16 = 48;
const G_COARSE: u16 = 51;
const G_FINE: u16 = 52;
const G_SAMPLEID: u16 = 53;
const G_MODES: u16 = 54;
const G_SCALE: u16 = 56;

/// Modulators of every instrument's global zone: they replace the SF2
/// default curves so that velocity, CC7 and CC11 scale the amplitude
/// linearly as the SPU2 driver does (the defaults' concave 960 cB curve is
/// amplitude squared; half of it is linear), and switch off the default
/// velocity-to-filter cutoff (the SPU2 has no filter).
const MODS: [(u16, u16, i16, u16, u16); 4] = [
    (0x0502, G_ATTEN, 480, 0, 0),
    (0x0587, G_ATTEN, 480, 0, 0),
    (0x058b, G_ATTEN, 480, 0, 0),
    (0x0102, 8, 0, 0x0c02, 0),
];

/// Writes banks into one SoundFont.
pub fn write(path: &Path, name: &str, banks: &[&Bank]) -> Result<()> {
    // sample data
    let mut smpl = Buf(Vec::new());
    let mut shdr = Buf(Vec::new());
    let mut sample_base = Vec::new();
    let mut n_samples = 0usize;
    for b in banks {
        sample_base.push(n_samples);
        for s in &b.samples {
            let s = &sf2_layout(s);
            let start = smpl.0.len() / 2;
            for &v in &s.pcm {
                smpl.i16(v);
            }
            // a loop is followed by its first points (SF2: 8 valid points
            // after the loop end, part of the sample)
            if let Some((ls, _)) = s.looped {
                for i in 0..GUARD {
                    smpl.i16(s.pcm.get(ls + i).copied().unwrap_or(0));
                }
            }
            let end = start + s.pcm.len() + if s.looped.is_some() { GUARD } else { 0 };
            for _ in 0..PAD {
                smpl.i16(0);
            }
            // a sample without loop still gets loop points inside it (unused)
            let pad = 8.min(s.pcm.len() / 4);
            let (l0, l1) = s.looped.map_or((start + pad, start + s.pcm.len() - pad), |(a, b)| (start + a, start + b));
            shdr.name(&s.name);
            shdr.u32(start as u32);
            shdr.u32(end as u32);
            shdr.u32(l0 as u32);
            shdr.u32(l1 as u32);
            shdr.u32(s.rate);
            shdr.u8(s.root);
            shdr.u8(0);
            shdr.u16(0);
            shdr.u16(1); // mono
            n_samples += 1;
        }
    }
    shdr.name("EOS");
    for _ in 0..(4 * 5 + 2 + 2 + 2) {
        shdr.u8(0);
    }

    let (mut inst, mut ibag, mut imod, mut igen) = (Buf(Vec::new()), Buf(Vec::new()), Buf(Vec::new()), Buf(Vec::new()));
    let (mut phdr, mut pbag, mut pgen) = (Buf(Vec::new()), Buf(Vec::new()), Buf(Vec::new()));
    let (mut nbag, mut ngen, mut nmod) = (0u16, 0u16, 0u16);
    let (mut npbag, mut npgen) = (0u16, 0u16);
    let mut ninst = 0u16;
    let put = |g: &mut Buf, n: &mut u16, op: u16, amt: i16| {
        g.u16(op);
        g.i16(amt);
        *n += 1;
    };
    let range = |g: &mut Buf, n: &mut u16, op: u16, lo: u8, hi: u8| {
        g.u16(op);
        g.u8(lo);
        g.u8(hi);
        *n += 1;
    };
    let mut presets: Vec<(u16, u8, String, u16)> = Vec::new();
    for (bi, b) in banks.iter().enumerate() {
        for ins in &b.instruments {
            if ins.zones.is_empty() {
                continue;
            }
            inst.name(&ins.name);
            inst.u16(nbag);
            // global zone: the modulators
            ibag.u16(ngen);
            ibag.u16(nmod);
            nbag += 1;
            for m in MODS {
                imod.u16(m.0);
                imod.u16(m.1);
                imod.i16(m.2);
                imod.u16(m.3);
                imod.u16(m.4);
                nmod += 1;
            }
            for z in &ins.zones {
                ibag.u16(ngen);
                ibag.u16(nmod);
                nbag += 1;
                range(&mut igen, &mut ngen, G_KEYRANGE, z.keys.0, z.keys.1);
                range(&mut igen, &mut ngen, G_VELRANGE, z.vels.0, z.vels.1);
                let (coarse, fine) = (z.tune_cents.div_euclid(100), z.tune_cents.rem_euclid(100));
                let (coarse, fine) = if fine > 50 { (coarse + 1, fine - 100) } else { (coarse, fine) };
                if coarse != 0 {
                    put(&mut igen, &mut ngen, G_COARSE, coarse.clamp(-120, 120) as i16);
                }
                if fine != 0 {
                    put(&mut igen, &mut ngen, G_FINE, fine as i16);
                }
                if z.fixed {
                    put(&mut igen, &mut ngen, G_SCALE, 0);
                }
                if z.pan != 0 {
                    put(&mut igen, &mut ngen, G_PAN, z.pan);
                }
                if z.atten != 0 {
                    put(&mut igen, &mut ngen, G_ATTEN, z.atten);
                }
                if z.reverb {
                    put(&mut igen, &mut ngen, G_REVERB, 1000);
                }
                put(&mut igen, &mut ngen, G_ATTACK, z.env.attack);
                put(&mut igen, &mut ngen, G_DECAY, z.env.decay);
                put(&mut igen, &mut ngen, G_SUSTAIN, z.env.sustain);
                put(&mut igen, &mut ngen, G_RELEASE, z.env.release);
                let s = &b.samples[z.sample];
                if s.looped.is_some() {
                    put(&mut igen, &mut ngen, G_MODES, 1);
                }
                put(&mut igen, &mut ngen, G_SAMPLEID, (sample_base[bi] + z.sample) as i16);
            }
            presets.push((b.number, ins.program, ins.name.clone(), ninst));
            ninst += 1;
        }
    }
    inst.name("EOI");
    inst.u16(nbag);
    ibag.u16(ngen);
    ibag.u16(nmod);
    for _ in 0..10 {
        imod.u8(0);
    }
    igen.u32(0);
    // presets sorted by bank and program
    presets.sort_by_key(|p| (p.0, p.1));
    for (bank, prog, name, ii) in &presets {
        phdr.name(name);
        phdr.u16(u16::from(*prog));
        phdr.u16(*bank);
        phdr.u16(npbag);
        phdr.u32(0);
        phdr.u32(0);
        phdr.u32(0);
        pbag.u16(npgen);
        pbag.u16(0);
        npbag += 1;
        put(&mut pgen, &mut npgen, G_INSTRUMENT, *ii as i16);
    }
    phdr.name("EOP");
    phdr.u16(0);
    phdr.u16(0);
    phdr.u16(npbag);
    phdr.u32(0);
    phdr.u32(0);
    phdr.u32(0);
    pbag.u16(npgen);
    pbag.u16(0);
    pgen.u32(0);
    let pmod = vec![0u8; 10];

    let info = riff::list(
        b"INFO",
        &[
            riff::chunk(b"ifil", &[2, 0, 1, 0]),
            riff::chunk(b"isng", &riff::zstr("EMU8000")),
            riff::chunk(b"INAM", &riff::zstr(name)),
            riff::chunk(b"ISFT", &riff::zstr("FFX Music Studio")),
            riff::chunk(b"ICMT", &riff::zstr("Made from the player's own PS2 disc of Final Fantasy X. Not for distribution.")),
        ],
    );
    let sdta = riff::list(b"sdta", &[riff::chunk(b"smpl", &smpl.0)]);
    let pdta = riff::list(
        b"pdta",
        &[
            riff::chunk(b"phdr", &phdr.0),
            riff::chunk(b"pbag", &pbag.0),
            riff::chunk(b"pmod", &pmod),
            riff::chunk(b"pgen", &pgen.0),
            riff::chunk(b"inst", &inst.0),
            riff::chunk(b"ibag", &ibag.0),
            riff::chunk(b"imod", &imod.0),
            riff::chunk(b"igen", &igen.0),
            riff::chunk(b"shdr", &shdr.0),
        ],
    );
    let _ = n_samples;
    riff::write_riff(path, b"sfbk", &[info, sdta, pdta])
}
