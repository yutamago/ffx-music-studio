//! FFX Music Studio: plays the PS2 music of the player's own FFX disc
//! through the converter's port of the PS2 sound driver, and edits it in
//! memory (`docs/studio.md`).
#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod audio;
mod edit;
mod export;
mod library;
mod midi;
mod player;
mod prefs;
mod rack;
mod roll;
mod seq;
mod session;
mod survey;
mod view;
#[cfg(test)]
mod tests;

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use demaster::spu2::Interp;

use crate::library::Library;
use crate::player::{Player, REVERB_NAMES};
use crate::seq::NoteSpec;
use crate::session::Session;

const USAGE: &str = "FFX Music Studio
usage:
  ffx-music-studio [--config <demaster.toml>]
      the studio (reads the disc named in demaster.toml)
  ffx-music-studio [--config <file>] [--open <track>] [--tab rack|roll|mixer|chip|song]
                   [--seek <tick>] [--paused] [--master <x>] [--output <x>]
                   [--channel <ch>] [--hold-key <key>]
      the studio, starting with a track open (--master: the track's volume;
      --output: the output volume for this run; --hold-key: a key held on
      the piano roll's keyboard, for screenshots)
  ffx-music-studio [--config <file>] --list
      the disc's tracks
  ffx-music-studio [--config <file>] --survey
      per track: channels with notes, notes, length, bank, the longest samples
  ffx-music-studio [--config <file>] --export <dir> [--only <track>]... [--no-renders]
                   [--no-samples] [--loops <n>] [--max-seconds <s>] [--threads <n>]
      the music for other DAWs: MIDI, SoundFonts, WAV samples, reverb impulse
      responses, reference renders and HOW-TO-OPEN.md (README, \"Export\")
  ffx-music-studio [--config <file>] --render <track> <seconds> <out.wav> [edits...]
      renders through the studio's playback path (track: id, musicNNN or part of
      the name). Edits:
        --mute <ch>  --solo <ch>  --gain <ch> <x>  --pan <ch> <offset>
        --transpose <semitones>  --transpose-ch <ch> <semitones>
        --instrument <ch> <bank>:<instrument>
        --tempo <factor>  --speed <factor>  --master <x>
        --reverb <0-9|name>  --wet <x>  --dry <x>  --interp gaussian|linear|nearest
        --loop <tick> <tick>  --no-loop  --start <tick>
        --delete-notes <ch>  --move-notes <ch> <ticks> <semitones>
        --add-note <ch> <tick> <length> <key> <velocity>
        --normalize      every track re-encoded with explicit notes
        --raw            the chip's output without the +9 dB and the limiter
  channels (tracks) count from 0, as in docs/music.md";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<()> {
    let mut config: Option<PathBuf> = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => {
                config = Some(args.get(i + 1).context("--config needs a file")?.into());
                i += 1;
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => rest.push(args[i].clone()),
        }
        i += 1;
    }
    match rest.first().map(String::as_str) {
        None => app::run(config, app::Start::default()),
        Some(a) if ["--open", "--tab", "--seek", "--paused", "--master", "--output", "--channel", "--hold-key"].contains(&a) => {
            app::run(config, app::Start::parse(&rest)?)
        }
        Some("--list") => {
            let lib = Library::open(config.as_deref())?;
            for t in &lib.tracks {
                println!("{}\t{}\t{}", t.id, t.pc_name, t.title);
            }
            Ok(())
        }
        Some("--render") => render(config, &rest[1..]),
        Some("--export") => {
            let dir = PathBuf::from(rest.get(1).context("--export <dir>")?);
            let mut o = export::Opts::default();
            let mut i = 2;
            while i < rest.len() {
                let val = |i: usize| rest.get(i + 1).with_context(|| format!("{} needs a value", rest[i]));
                match rest[i].as_str() {
                    "--only" => {
                        o.only.push(val(i)?.clone());
                        i += 1;
                    }
                    "--no-renders" => o.renders = false,
                    "--no-samples" => o.samples = false,
                    "--loops" => {
                        o.loops = val(i)?.parse::<usize>()?.max(1);
                        i += 1;
                    }
                    "--max-seconds" => {
                        o.max_seconds = val(i)?.parse()?;
                        i += 1;
                    }
                    "--threads" => {
                        o.threads = val(i)?.parse()?;
                        i += 1;
                    }
                    other => bail!("unknown export option {other}\n{USAGE}"),
                }
                i += 1;
            }
            export::run(&mut Library::open(config.as_deref())?, &dir, &o)
        }
        Some("--survey") => match rest.get(1) {
            Some(q) => survey::notes(&mut Library::open(config.as_deref())?, q),
            None => survey::run(&mut Library::open(config.as_deref())?),
        },
        Some(other) => bail!("unknown argument {other}\n{USAGE}"),
    }
}

fn render(config: Option<PathBuf>, a: &[String]) -> Result<()> {
    if a.len() < 3 {
        bail!("{USAGE}");
    }
    let mut lib = Library::open(config.as_deref())?;
    let info = lib.find(&a[0]).with_context(|| format!("no track {} on the disc", a[0]))?.clone();
    let seconds: f64 = a[1].parse().context("seconds")?;
    let out = PathBuf::from(&a[2]);
    let (bytes, wd, missing) = lib.song(info.id)?;
    if let Some(m) = missing {
        eprintln!("warning: {m}");
    }
    let mut s = Session::new(info.id, info.label(), &bytes, wd)?;
    let mut start = 0u32;
    let mut raw = false;
    let mut normalize = false;
    let mut i = 3;
    let num = |i: usize| -> Result<f64> {
        a.get(i).with_context(|| format!("{} needs a value", a[i - 1]))?.parse::<f64>().with_context(|| a[i].clone())
    };
    let ch = |i: usize, s: &Session| -> Result<usize> {
        let c = num(i)? as usize;
        if c >= s.channels.len() {
            bail!("channel {c}: the track has {} channels (0..{})", s.channels.len(), s.channels.len() - 1);
        }
        Ok(c)
    };
    while i < a.len() {
        match a[i].as_str() {
            "--mute" => {
                let c = ch(i + 1, &s)?;
                s.channels[c].mute = true;
                i += 1;
            }
            "--solo" => {
                let c = ch(i + 1, &s)?;
                s.channels[c].solo = true;
                i += 1;
            }
            "--gain" => {
                let c = ch(i + 1, &s)?;
                s.channels[c].gain = num(i + 2)? as f32;
                i += 2;
            }
            "--pan" => {
                let c = ch(i + 1, &s)?;
                s.channels[c].pan = num(i + 2)? as i32;
                i += 2;
            }
            "--transpose" => {
                let n = num(i + 1)? as i32;
                for c in &mut s.channels {
                    c.transpose = n;
                }
                i += 1;
            }
            "--transpose-ch" => {
                let c = ch(i + 1, &s)?;
                s.channels[c].transpose = num(i + 2)? as i32;
                i += 2;
            }
            "--instrument" => {
                let c = ch(i + 1, &s)?;
                let spec = a.get(i + 2).context("--instrument <ch> <bank>:<instrument>")?;
                let (b, n) = spec.split_once(':').context("<bank>:<instrument>")?;
                let (b, n): (u16, usize) = (b.parse()?, n.parse()?);
                let bank = lib.bank(b)?;
                if n >= bank.instruments.len() {
                    bail!("bank {b} has {} instruments", bank.instruments.len());
                }
                s.set_instrument(c, Some((b, &bank, n)));
                i += 2;
            }
            "--tempo" => {
                s.tempo = num(i + 1)?;
                i += 1;
            }
            "--speed" => {
                s.speed = num(i + 1)?;
                i += 1;
            }
            "--master" => {
                s.master = num(i + 1)? as f32;
                i += 1;
            }
            "--reverb" => {
                let v = a.get(i + 1).context("--reverb <mode>")?;
                let m = v.parse::<u8>().ok().or_else(|| REVERB_NAMES.iter().position(|n| n.replace(' ', "") == v.replace(' ', "").to_lowercase()).map(|p| p as u8));
                s.chip.reverb = Some(m.filter(|&m| m < 10).with_context(|| format!("reverb mode {v}"))?);
                i += 1;
            }
            "--wet" => {
                s.chip.wet = num(i + 1)? as f32;
                i += 1;
            }
            "--dry" => {
                s.chip.dry = num(i + 1)? as f32;
                i += 1;
            }
            "--interp" => {
                s.chip.interp = match a.get(i + 1).map(String::as_str) {
                    Some("gaussian") => Interp::Gaussian,
                    Some("linear") => Interp::Linear,
                    Some("nearest") => Interp::Nearest,
                    _ => bail!("--interp gaussian|linear|nearest"),
                };
                i += 1;
            }
            "--loop" => {
                s.set_loop(Some((num(i + 1)? as u32, num(i + 2)? as u32)));
                i += 2;
            }
            "--no-loop" => s.looping = false,
            "--start" => {
                start = num(i + 1)? as u32;
                i += 1;
            }
            "--delete-notes" => {
                let c = ch(i + 1, &s)?;
                for n in s.seq.notes(c) {
                    s.seq.set_note(c, n.id, None);
                }
                i += 1;
            }
            "--move-notes" => {
                let c = ch(i + 1, &s)?;
                let (dt, dk) = (num(i + 2)? as i64, num(i + 3)? as i64);
                for n in s.seq.notes(c) {
                    let spec = NoteSpec {
                        start: (i64::from(n.start) + dt).max(0) as u32,
                        len: n.len,
                        key: (i64::from(n.key) + dk).clamp(0, 127) as u8,
                        vel: n.vel,
                    };
                    s.seq.set_note(c, n.id, Some(spec));
                }
                i += 3;
            }
            "--add-note" => {
                let c = ch(i + 1, &s)?;
                let spec = NoteSpec {
                    start: num(i + 2)? as u32,
                    len: num(i + 3)? as u32,
                    key: num(i + 4)? as u8,
                    vel: num(i + 5)? as u8,
                };
                s.seq.add_note(c, spec);
                i += 5;
            }
            "--normalize" => normalize = true,
            "--raw" => raw = true,
            other => bail!("unknown edit {other}\n{USAGE}"),
        }
        i += 1;
    }
    if normalize {
        let flags = s.seq.flags();
        for t in &mut s.seq.tracks {
            t.normalize(flags);
        }
    }
    let mut p = Player::new(lib.tables.clone(), s.bgm()?, s.wd.clone(), demaster::spu2::RATE);
    p.raw = raw;
    p.set_settings(s.settings());
    p.restart();
    if start > 0 {
        p.seek_tick(start);
    }
    let frames = (seconds * f64::from(demaster::spu2::RATE)) as usize;
    let t0 = std::time::Instant::now();
    let mut w = demaster::musicjob::WavWriter::create(&out, demaster::spu2::RATE)?;
    let mut buf = Vec::new();
    let mut left = frames;
    let mut peak = 0i32;
    while left > 0 {
        let n = left.min(48_000);
        buf.clear();
        p.render_i16(n, &mut buf);
        peak = peak.max(buf.iter().map(|s| i32::from(s[0]).abs().max(i32::from(s[1]).abs())).max().unwrap_or(0));
        w.write(&buf)?;
        left -= n;
    }
    w.finish()?;
    let marks = &p.song().marks;
    println!(
        "{}\t{} frames\tpeak {peak}\tloop point {:?}\tjumps {:?}\tend {:?}\t{:.1}s",
        info.label(),
        frames,
        marks.loop_point,
        marks.loop_jumps,
        marks.end,
        t0.elapsed().as_secs_f64()
    );
    Ok(())
}
