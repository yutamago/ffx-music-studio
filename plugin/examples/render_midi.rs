//! Renders a MIDI file through the plugin's processing, as a DAW would play
//! it (blocks of 512 samples), and compares it with a reference render.
//!
//! render-midi [--source <toml|export dir>] --track <id> [--bank <n>] [--block <n>]
//!             [--rate <hz>] [--seconds <s>] [--host-tempo] <file.mid> [--out <out.wav>] [--compare <ref.wav>]

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use ffx_spu2_plugin::engine::{Engine, Instrument, Settings, TempoSource};
use ffx_spu2_plugin::offline::{Host, Smf, render};
use ffx_spu2_plugin::source::Library;

fn read_wav(p: &str) -> Result<Vec<[i16; 2]>> {
    let b = std::fs::read(p).with_context(|| p.to_string())?;
    let mut o = 12;
    while o + 8 <= b.len() {
        let id = &b[o..o + 4];
        let len = u32::from_le_bytes(b[o + 4..o + 8].try_into()?) as usize;
        if id == b"data" {
            let d = &b[o + 8..(o + 8 + len).min(b.len())];
            return Ok(d.chunks_exact(4).map(|c| [i16::from_le_bytes([c[0], c[1]]), i16::from_le_bytes([c[2], c[3]])]).collect());
        }
        o += 8 + len + (len & 1);
    }
    bail!("{p}: no data chunk")
}

fn write_wav(p: &str, s: &[[i16; 2]]) -> Result<()> {
    let mut b = Vec::new();
    let n = (s.len() * 4) as u32;
    b.extend(b"RIFF");
    b.extend((36 + n).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(48000u32.to_le_bytes());
    b.extend((48000u32 * 4).to_le_bytes());
    b.extend(4u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(n.to_le_bytes());
    for f in s {
        b.extend(f[0].to_le_bytes());
        b.extend(f[1].to_le_bytes());
    }
    std::fs::write(p, b)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut source, mut track, mut bank, mut block, mut rate, mut seconds) = (String::new(), None, None, 512usize, 48000.0, None);
    let (mut midi, mut out, mut compare, mut host) = (None, None, None, false);
    let mut log_at: Option<u64> = None;
    let mut lockstep = false;
    let mut until: Option<f64> = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let mut val = || {
            i += 1;
            args.get(i).cloned().context("missing value")
        };
        match a {
            "--source" => source = val()?,
            "--track" => track = Some(val()?.trim_start_matches("music").parse::<u16>()?),
            "--bank" => bank = Some(val()?.parse::<usize>()?),
            "--block" => block = val()?.parse()?,
            "--rate" => rate = val()?.parse()?,
            "--seconds" => seconds = Some(val()?.parse::<f64>()?),
            "--out" => out = Some(val()?),
            "--compare" => compare = Some(val()?),
            "--host-tempo" => host = true,
            "--lockstep" => lockstep = true,
            "--until" => until = Some(val()?.parse::<f64>()?),
            "--log-at" => log_at = Some(val()?.parse::<u64>()?),
            _ => midi = Some(a.to_string()),
        }
        i += 1;
    }
    let midi = midi.context("no MIDI file")?;
    let mut lib = Library::open(&source)?;
    let (wd_id, header, label) = match (track, bank) {
        (Some(t), _) => {
            let t = lib.track(t).with_context(|| format!("no track {t}"))?.clone();
            (t.wd, t.header, t.label())
        }
        (None, Some(b)) => (*lib.banks.get(b).context("no such bank")?, Default::default(), format!("bank {b}")),
        _ => bail!("--track or --bank"),
    };
    let wd = lib.bank(wd_id)?;
    if let (Some(f), Some(t)) = (log_at, track) {
        // the driver's own log around output frame `f` (a disc source)
        let b = lib.sequence(t).context("--log-at needs a disc source")?;
        let bgm = Arc::new(demaster::sqseq::Bgm::parse(&b)?);
        let mut spu = demaster::spu2::Spu2::new(wd.samples.clone());
        let mut song = demaster::sqseq::Song::new(lib.tables.clone(), bgm, wd.clone(), &mut spu);
        song.log = Some(Vec::new());
        while song.frame() < f + 4000 && song.playing() {
            song.interrupt_skip(&mut spu);
        }
        for e in song.log.as_ref().expect("log") {
            if e.frame + 4000 >= f && e.frame <= f + 2000 {
                println!("frame {} tick {} track {} {:?}", e.frame, e.tick, e.track, e.event);
            }
        }
        return Ok(());
    }
    let inst = Arc::new(Instrument::new(lib.tables.clone(), wd, header, label.clone()));
    let settings = Settings { tempo: if host { TempoSource::Host } else { TempoSource::Sequence }, ..Settings::default() };
    let mut eng = Engine::new(inst, rate, settings);
    let smf = Smf::parse(&std::fs::read(&midi)?)?;
    let lat = eng.latency() as usize;
    if lockstep {
        // the driver from the sequence next to the engine, interrupt by
        // interrupt: the first where the voices differ
        let t = track.context("--lockstep needs --track")?;
        let b = lib.sequence(t).context("--lockstep needs a disc source")?;
        let bgm = Arc::new(demaster::sqseq::Bgm::parse(&b)?);
        let mut spu = demaster::spu2::Spu2::new(eng.instrument().wd.samples.clone());
        let mut song = demaster::sqseq::Song::new(lib.tables.clone(), bgm, eng.instrument().wd.clone(), &mut spu);
        let start = (ffx_spu2_plugin::engine::LOOKAHEAD_S * 48000.0) as usize / 200;
        let mut sink = Vec::new();
        let total = (seconds.unwrap_or(60.0) * 240.0) as usize;
        let mut host = Host::new(&smf, rate);
        let mut scratch = Vec::new();
        for b in 0..total + start {
            host.block(&mut eng, 200, &mut scratch);
            if b < start {
                continue;
            }
            song.interrupt(&mut spu, &mut sink);
            let (a, c) = (eng.song().voice_tracks(), song.voice_tracks());
            let ea: Vec<(i16, u32, u32)> = (0..48).map(|i| (eng.spu().envx(i), eng.spu().voice_level(i).to_bits(), eng.spu().nax(i))).collect();
            let ec: Vec<(i16, u32, u32)> = (0..48).map(|i| (spu.envx(i), spu.voice_level(i).to_bits(), spu.nax(i))).collect();
            let busy = |v: &[Option<usize>]| v.iter().map(Option::is_some).collect::<Vec<_>>();
            if busy(&a) != busy(&c) || ea != ec {
                let i = b - start;
                println!("interrupt {i} (frame {}, tick {}/{}): voices differ", i * 200, eng.song().ticks_run(), song.ticks_run());
                for v in 0..48 {
                    if a[v].is_some() != c[v].is_some() || ea[v] != ec[v] {
                        println!(
                            "  voice {v}: plugin {:?} env {} level {} nax {:x} | driver {:?} env {} level {} nax {:x}",
                            a[v],
                            ea[v].0,
                            f32::from_bits(ea[v].1),
                            ea[v].2,
                            c[v],
                            ec[v].0,
                            f32::from_bits(ec[v].1),
                            ec[v].2
                        );
                    }
                }
                return Ok(());
            }
        }
        println!("no difference in {} interrupts", total);
        return Ok(());
    }
    let reference = compare.as_deref().map(read_wav).transpose()?;
    let len_s = seconds.unwrap_or_else(|| smf.tick_seconds(smf.end_tick as f64) + 2.0);
    let mut frames = (len_s * rate) as usize;
    if let Some(r) = &reference
        && seconds.is_none()
    {
        frames = frames.min(r.len());
    }
    let t0 = std::time::Instant::now();
    let got = render(&mut eng, &smf, rate, block, frames + lat);
    let el = t0.elapsed().as_secs_f64();
    let pcm: Vec<[i16; 2]> = got[lat..]
        .iter()
        .map(|s| [(s[0] * 32768.0).round().clamp(-32768.0, 32767.0) as i16, (s[1] * 32768.0).round().clamp(-32768.0, 32767.0) as i16])
        .collect();
    println!("{label}: {:.1} s rendered in {el:.1} s, latency {lat}, late events {}", frames as f64 / rate, eng.late);
    if let Some(o) = &out {
        write_wav(o, &pcm)?;
    }
    if let Some(r) = reference {
        let mut n = pcm.len().min(r.len());
        if let Some(u) = until {
            // a looping song: the file holds one pass, the reference two
            n = n.min((u * 48000.0) as usize);
        }
        let (mut maxd, mut sum, mut first) = (0i32, 0f64, None);
        let mut sig = 0f64;
        for k in 0..n {
            for c in 0..2 {
                let d = i32::from(pcm[k][c]) - i32::from(r[k][c]);
                if d != 0 && first.is_none() {
                    first = Some(k);
                }
                maxd = maxd.max(d.abs());
                sum += f64::from(d) * f64::from(d);
                sig += f64::from(r[k][c]) * f64::from(r[k][c]);
            }
        }
        let rms = (sum / (2 * n) as f64).sqrt();
        let srms = (sig / (2 * n) as f64).sqrt();
        println!(
            "compare: {n} frames ({:.2} s), identical: {}, max diff {maxd}, rms diff {rms:.3} (signal rms {srms:.1}), first diff at {}",
            n as f64 / 48000.0,
            maxd == 0,
            first.map_or("-".into(), |f| format!("frame {f} ({:.3} s)", f as f64 / 48000.0))
        );
    }
    Ok(())
}
