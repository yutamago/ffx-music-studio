//! `--export <dir>`: the disc's music for other DAWs (README, "Export"):
//! MIDI files from the driver's own playback, the WD banks as SoundFonts and
//! WAV samples, the reverb presets as impulse responses with the per-track
//! settings, reference renders and `HOW-TO-OPEN.md`.

pub mod riff;
pub mod sf2;
pub mod smf;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use demaster::spu2::{self, Spu2};
use demaster::sqseq::{Bgm, Tables, Wd};
use serde_json::{Value, json};

use crate::library::{Library, TrackInfo};
use crate::player::{Player, REVERB_NAMES, Settings};

/// What to export.
#[derive(Debug, Clone)]
pub struct Opts {
    /// Tracks (ids, names), empty: all.
    pub only: Vec<String>,
    pub renders: bool,
    pub samples: bool,
    /// Loops of a looping track in its reference render.
    pub loops: usize,
    /// Upper bound of a render in seconds.
    pub max_seconds: f64,
    pub threads: usize,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { only: Vec::new(), renders: true, samples: true, loops: 2, max_seconds: 600.0, threads: 4 }
    }
}

/// `musicNNN-<name>` with the list's name in lower case, words joined by `-`.
pub fn file_stem(t: &TrackInfo) -> String {
    let mut s = String::new();
    for c in t.title.replace("(proposed)", "").chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c.to_ascii_lowercase());
        } else if !s.ends_with('-') {
            s.push('-');
        }
    }
    let s = s.trim_matches('-');
    if s.is_empty() { format!("music{:03}", t.id) } else { format!("music{:03}-{s}", t.id) }
}

/// The reverb presets' register names (libsd's order).
const REG_NAMES: [&str; 32] = [
    "dAPF1", "dAPF2", "vIIR", "vCOMB1", "vCOMB2", "vCOMB3", "vCOMB4", "vWALL", "vAPF1", "vAPF2", "mLSAME", "mRSAME", "mLCOMB1",
    "mRCOMB1", "mLCOMB2", "mRCOMB2", "dLSAME", "dRSAME", "mLDIFF", "mRDIFF", "mLCOMB3", "mRCOMB3", "mLCOMB4", "mRCOMB4", "dLDIFF",
    "dRDIFF", "mLAPF1", "mRAPF1", "mLAPF2", "mRAPF2", "vLIN", "vRIN",
];

/// The impulse response of reverb mode `mode`: a click (one ADPCM sample)
/// on a voice that only feeds the reverb, through `spu2`'s reverb at full
/// effect volume, divided by the same click played dry (its sum), so a
/// convolution of the dry signal with it is the SPU2's wet output at
/// effect volume 0x7fff. Trimmed where it falls below -100 dB.
pub fn impulse_response(tab: &Tables, mode: usize) -> Vec<[f32; 2]> {
    let mut ram = vec![0u8; 64];
    // block 0: shift 0, filter 0, sample 10 = 7 << 12, loop end without repeat
    ram[1] = 1;
    ram[2 + 5] = 0x07;
    let run = |wet: bool| -> Vec<[i16; 2]> {
        let mut spu = Spu2::new(ram.clone());
        spu.set_reverb(0, tab.reverb[mode]);
        spu.set_reverb(1, tab.reverb[mode]);
        spu.set_evol(0, 0x7fff, 0x7fff);
        spu.set_evol(1, 0x7fff, 0x7fff);
        spu.set_adsr(0, 0x000f, 0x1fc0);
        spu.set_volume(0, 0x3fff, 0x3fff);
        spu.set_pitch(0, 0x1000);
        spu.set_ssa(0, 0);
        spu.set_mix(0, !wet, !wet, wet, wet);
        spu.key_on(1);
        let mut out = Vec::new();
        spu.render(12 * spu2::RATE as usize, &mut out);
        out
    };
    let dry = run(false);
    let sum: f64 = dry.iter().map(|s| f64::from(s[0])).sum();
    let wet = run(true);
    let peak = wet.iter().map(|s| i32::from(s[0]).abs().max(i32::from(s[1]).abs())).max().unwrap_or(0);
    let floor = (f64::from(peak) * 1e-5).max(1.0) as i32;
    let last = wet.iter().rposition(|s| i32::from(s[0]).abs() > floor || i32::from(s[1]).abs() > floor).unwrap_or(0);
    let end = (last + spu2::RATE as usize / 10).min(wet.len());
    let k = if sum.abs() > 0.0 { 1.0 / sum } else { 0.0 };
    wet[..end].iter().map(|s| [(f64::from(s[0]) * k) as f32, (f64::from(s[1]) * k) as f32]).collect()
}

/// The engine's own render: a looping track to its `loops`-th jump back
/// (intro and `loops` passes), else to its end and 2 s of release and
/// reverb; 48 kHz, through the studio's player (gain and limiter).
pub fn render(tab: Arc<Tables>, bgm: Arc<Bgm>, wd: Arc<Wd>, path: &Path, loops: usize, max_seconds: f64) -> Result<f64> {
    if !bgm.loops() {
        return crate::player::export_wav(tab, bgm, wd, Settings::default(), path, max_seconds);
    }
    let mut p = Player::new(tab, bgm, wd, spu2::RATE);
    p.set_settings(Settings::default());
    p.restart();
    let mut w = demaster::musicjob::WavWriter::create(path, spu2::RATE)?;
    let mut buf = Vec::new();
    let mut frames = 0usize;
    let max = (max_seconds * f64::from(spu2::RATE)) as usize;
    while frames < max {
        buf.clear();
        p.render_i16(4800, &mut buf);
        let jumps = &p.song().marks.loop_jumps;
        if jumps.len() >= loops {
            let keep = (jumps[loops - 1] as usize).saturating_sub(frames).min(buf.len());
            w.write(&buf[..keep])?;
            frames += keep;
            break;
        }
        w.write(&buf)?;
        frames += buf.len();
    }
    w.finish()?;
    Ok(frames as f64 / f64::from(spu2::RATE))
}

pub fn run(lib: &mut Library, dir: &Path, opts: &Opts) -> Result<()> {
    let t0 = std::time::Instant::now();
    for d in ["midi", "soundfonts", "reverb", "renders", "samples"] {
        std::fs::create_dir_all(dir.join(d)).with_context(|| dir.join(d).display().to_string())?;
    }
    let tracks: Vec<TrackInfo> = if opts.only.is_empty() {
        let mut v = lib.tracks.clone();
        v.sort_by_key(|t| t.id);
        v
    } else {
        opts.only.iter().map(|q| lib.find(q).cloned().with_context(|| format!("no track {q}"))).collect::<Result<_>>()?
    };

    // the banks the sequences name (and the disc has), numbered in order:
    // the SF2 bank number of each (the same for every export of a disc)
    let mut music_banks = std::collections::BTreeSet::new();
    for t in &lib.tracks.clone() {
        let b = lib.src.sequence(&mut lib.disc, t.id)?;
        let wd = Bgm::parse(&b)?.wd;
        if lib.banks.contains(&wd) {
            music_banks.insert(wd);
        }
    }
    let number: BTreeMap<u16, u16> = music_banks.iter().enumerate().map(|(i, &w)| (w, i as u16)).collect();
    let mut bank_ids: Vec<u16> = Vec::new();
    let mut songs: Vec<(TrackInfo, Arc<Bgm>, Arc<Wd>, Option<String>)> = Vec::new();
    for t in &tracks {
        let (bytes, wd, missing) = lib.song(t.id)?;
        let bgm = Arc::new(Bgm::parse(&bytes)?);
        if missing.is_none() && !bank_ids.contains(&bgm.wd) {
            bank_ids.push(bgm.wd);
        }
        songs.push((t.clone(), bgm, wd, missing));
    }
    bank_ids.sort_unstable();
    bank_ids.dedup();

    // SoundFonts and samples
    let mut banks: Vec<sf2::Bank> = Vec::new();
    for &id in &bank_ids {
        let wd = lib.bank(id)?;
        let b = sf2::bank(&wd, number[&id]);
        if b.instruments.is_empty() {
            continue;
        }
        sf2::write(&dir.join("soundfonts").join(format!("wd{id:03}.sf2")), &format!("FFX PS2 WD {id:03}"), &[&b])?;
        if opts.samples {
            let sd = dir.join("samples").join(format!("wd{id:03}"));
            std::fs::create_dir_all(&sd)?;
            for (i, s) in b.samples.iter().enumerate() {
                sf2::write_wav(s, &sd.join(format!("{i:04}.wav")))?;
            }
        }
        banks.push(b);
    }
    // one combined font: possible while the numbers stay below 128 (SF2
    // bank 128 is percussion)
    let combined = number.len() <= 128;
    if combined && !banks.is_empty() && opts.only.is_empty() {
        let refs: Vec<&sf2::Bank> = banks.iter().collect();
        sf2::write(&dir.join("soundfonts").join("ffx-ps2.sf2"), "FFX PS2 music (all banks)", &refs)?;
    }
    eprintln!("soundfonts: {} banks, {:.1}s", banks.len(), t0.elapsed().as_secs_f64());

    // reverb impulse responses
    let mut presets = Vec::new();
    for (mode, name) in REVERB_NAMES.iter().enumerate().skip(1) {
        let p = lib.tables.reverb[mode];
        let file = format!("reverb/{mode}-{}.wav", name.replace(' ', "-"));
        let ir = impulse_response(&lib.tables, mode);
        riff::write_wav_f32(&dir.join(&file), spu2::RATE, &ir)?;
        let regs: serde_json::Map<String, Value> =
            REG_NAMES.iter().zip(p.regs).map(|(n, v)| (n.to_string(), json!(format!("0x{v:04x}")))).collect();
        presets.push(json!({
            "mode": mode, "name": name, "impulse_response": file,
            "ir_seconds": ir.len() as f64 / f64::from(spu2::RATE),
            "work_area_bytes": p.size * 8, "registers": regs,
        }));
    }

    // MIDI
    let mut rows = Vec::new();
    let mut reverb_tracks = Vec::new();
    for (t, bgm, wd, missing) in &songs {
        let log = smf::run_log(lib.tables.clone(), bgm.clone(), wd.clone(), 48 * 300 * 20);
        let stem = file_stem(t);
        let title = if t.title.is_empty() { format!("music{:03}", t.id) } else { t.title.clone() };
        let wdid = bgm.wd;
        let bno = number.get(&wdid).copied().unwrap_or(0);
        let names = |p: usize| format!("wd{wdid:03} b{bno} #{p}");
        let (bytes, info) = smf::build(bgm, number.get(&bgm.wd).copied().unwrap_or(0), &log, &title, &names);
        std::fs::write(dir.join("midi").join(format!("{stem}.mid")), bytes)?;
        let secs = |tick: u64| smf::tick_seconds(tick, &info.tempos);
        let sf = if missing.is_some() { String::new() } else { format!("wd{:03}.sf2", bgm.wd) };
        let mode = usize::from(bgm.reverb_mode);
        let depth = if bgm.reverb_depth != 0 { i32::from(bgm.reverb_depth) } else { 0x7fff };
        reverb_tracks.push(json!({
            "track": format!("music{:03}", t.id), "title": title, "midi": format!("midi/{stem}.mid"),
            "reverb_mode": if mode < 10 { json!(mode) } else { json!(null) },
            "reverb_preset": REVERB_NAMES.get(mode).copied().unwrap_or("off"),
            "impulse_response": if (1..10).contains(&mode) { json!(format!("reverb/{mode}-{}.wav", REVERB_NAMES[mode].replace(' ', "-"))) } else { json!(null) },
            "effect_volume": format!("0x{:04x}", depth as u16),
            // the effect volume once tick 0 is played (many songs set it there)
            "wet_gain": info.depths.iter().take_while(|d| d.0 == 0).last().map_or(f64::from(depth) / 32767.0, |d| f64::from(d.1) / 128.0),
            "header_wet_gain": f64::from(depth) / 32767.0,
            "depth_changes": info.depths.iter().map(|(tk, d)| json!({"tick": tk, "seconds": secs(*tk), "wet_gain": f64::from(*d) / 128.0})).collect::<Vec<_>>(),
            "channels_with_reverb_send": "CC91 = 127 in the MIDI (commands 0x60/0x62; regions with a reverb flag always send)",
        }));
        let tempo0 = info.tempos.first().map_or(1, |t| t.1);
        let tempo_list: Vec<String> = {
            let mut v: Vec<u8> = info.tempos.iter().map(|t| t.1).collect();
            v.dedup();
            v.iter().map(|b| b.to_string()).collect()
        };
        rows.push(json!({
            "id": t.id, "stem": stem, "title": title, "soundfont": sf, "wd": bgm.wd,
            "midi_bank": number.get(&bgm.wd),
            "missing_bank": missing, "tempo": tempo0, "tempos": tempo_list.join("/"),
            "loop_start": info.loop_start, "loop_end": info.loop_end,
            "loop_start_s": info.loop_start.map(secs), "loop_end_s": info.loop_end.map(secs),
            "length_s": secs(info.ticks), "notes": info.notes, "dropped_notes": info.dropped,
            "timing_max_dev_ms": (info.timing_dev_ms * 10.0).round() / 10.0, "repeats": info.repeats,
            "reverb": REVERB_NAMES.get(mode).copied().unwrap_or("off"),
            "channels": info.tracks.iter().filter(|c| c.notes > 0).count(),
            "ports": info.tracks.iter().map(|c| c.port).max().unwrap_or(0) + 1,
        }));
    }
    eprintln!("midi: {} files, {:.1}s", rows.len(), t0.elapsed().as_secs_f64());
    let rv = json!({
        "about": "The SPU2 reverb per track (README of FFX Music Studio, 'Export'). impulse_response: 32-bit float stereo WAV, the wet output for a dry input at unit gain and effect volume 0x7fff; scale its level by wet_gain.",
        "presets": presets, "tracks": reverb_tracks,
    });
    std::fs::write(dir.join("reverb.json"), serde_json::to_string_pretty(&rv)?)?;
    std::fs::write(dir.join("tracks.json"), serde_json::to_string_pretty(&rows)?)?;

    // HOW-TO-OPEN.md with the track table
    let mut table = String::from("| MIDI file | SoundFont | SF2/MIDI bank (CC0) | tempo (BPM) | loop (s) | channels | reverb |\n|---|---|---|---|---|---|---|\n");
    for r in &rows {
        let lp = match (r["loop_start_s"].as_f64(), r["loop_end_s"].as_f64()) {
            (Some(a), Some(b)) => format!("{a:.2} - {b:.2}"),
            _ => "none".into(),
        };
        let sf = r["soundfont"].as_str().unwrap_or("");
        let ports = r["ports"].as_u64().unwrap_or(1);
        table.push_str(&format!(
            "| `{}.mid` | {} | {} | {} | {} | {}{} | {} |\n",
            r["stem"].as_str().unwrap_or(""),
            if sf.is_empty() { "(bank not on the disc)".to_string() } else { format!("`{sf}`") },
            r["midi_bank"].as_u64().map_or("-".to_string(), |b| b.to_string()),
            r["tempos"].as_str().unwrap_or(""),
            lp,
            r["channels"],
            if ports > 1 { format!(" ({ports} ports)") } else { String::new() },
            r["reverb"].as_str().unwrap_or(""),
        ));
    }
    let how = include_str!("HOW-TO-OPEN.md").replace("{{TRACKS}}", &table).replace("{{COMBINED}}", if combined {
        "`soundfonts/ffx-ps2.sf2` holds every bank at once, at the same bank numbers."
    } else {
        "There is no combined font on this disc (a bank number is 128 or more)."
    });
    std::fs::write(dir.join("HOW-TO-OPEN.md"), how)?;

    // reference renders, in parallel
    if opts.renders {
        let jobs: Vec<(PathBuf, Arc<Bgm>, Arc<Wd>)> = songs
            .iter()
            .filter(|s| s.3.is_none())
            .map(|(t, b, w, _)| (dir.join("renders").join(format!("{}.wav", file_stem(t))), b.clone(), w.clone()))
            .collect();
        let tables = lib.tables.clone();
        let next = std::sync::atomic::AtomicUsize::new(0);
        let results: std::sync::Mutex<BTreeMap<usize, Result<f64>>> = std::sync::Mutex::new(BTreeMap::new());
        std::thread::scope(|sc| {
            for _ in 0..opts.threads.max(1) {
                sc.spawn(|| {
                    loop {
                        let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        let Some((p, b, w)) = jobs.get(i) else { break };
                        let r = render(tables.clone(), b.clone(), w.clone(), p, opts.loops, opts.max_seconds);
                        results.lock().expect("lock").insert(i, r);
                    }
                });
            }
        });
        for (i, r) in results.into_inner().expect("lock") {
            r.with_context(|| jobs[i].0.display().to_string())?;
        }
        eprintln!("renders: {} files, {:.1}s", jobs.len(), t0.elapsed().as_secs_f64());
    }
    println!("exported to {} in {:.1}s", dir.display(), t0.elapsed().as_secs_f64());
    Ok(())
}
