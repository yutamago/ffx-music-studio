//! Tests of the playback path: the round trip (decoded and re-encoded
//! sequences play exactly as the original), edits change the output as
//! they should, the loop markers. The synthetic ones need no disc; the disc
//! ones run when `FFX_STUDIO_CONFIG` names a `demaster.toml` (else they say
//! so and pass).

use std::sync::Arc;

use demaster::spu2::RATE;
use demaster::sqseq::{Bgm, Tables, Wd};

use crate::player::{Live, Player};
use demaster::sqseq::TrackMod;
use crate::seq::tests::bgm;
use crate::seq::{NoteSpec, Seq};
use crate::session::Session;

/// A bank of one instrument: a square wave of 28 samples (1714 Hz at key
/// 60), instant attack, full sustain, fast release.
fn wd() -> Arc<Wd> {
    wd_release(0)
}

/// The same with release rate `rr` (ADSR2 bits 0-4, linear; 0x0c: about
/// 0.17 s from full level).
fn wd_release(rr: u32) -> Arc<Wd> {
    let mut b = vec![0u8; 0x20];
    b[..2].copy_from_slice(b"WD");
    b[4..8].copy_from_slice(&16u32.to_le_bytes()); // sample bytes
    b[8..12].copy_from_slice(&1u32.to_le_bytes()); // instruments
    b[12..16].copy_from_slice(&1u32.to_le_bytes()); // regions
    b.extend_from_slice(&0x30u32.to_le_bytes()); // first region
    b.resize(0x30, 0);
    let mut r = vec![0u8; 0x20];
    r[0..4].copy_from_slice(&0x300u32.to_le_bytes());
    r[12..16].copy_from_slice(&(0x1fc0_000fu32 | (rr & 0x1f) << 16).to_le_bytes());
    r[0x14] = 127;
    r[0x15] = 127;
    r[0x16] = 127;
    b.extend(r);
    // one ADPCM block, loops on itself: filter 0, shift 0, +7 x14, -7 x14
    let mut blk = vec![0x00u8, 0x07];
    blk.extend([0x77u8; 7]);
    blk.extend([0x99u8; 7]);
    b.extend(blk);
    Arc::new(Wd::parse(&b).expect("synthetic WD"))
}

fn tables() -> Arc<Tables> {
    Arc::new(Tables::generic())
}

/// Tempo 120 (96 ticks a second); track 1 plays key 60 at ticks 0, 96, 192,
/// 288 for 48 ticks each; track 2 key 72 at 48 and 240; the song loops from
/// tick 96 at its end (tick 384).
fn song() -> Vec<u8> {
    let note = |k: u8, len: u32, rest: u32| vec![(0x11, vec![k, 100], len), (0x1a, vec![k], rest)];
    let mut t1 = vec![(0x20u8, vec![0u8], 0u32)];
    t1.extend(note(60, 48, 48));
    t1.push((0x02, vec![], 0));
    for _ in 0..3 {
        t1.extend(note(60, 48, 48));
    }
    t1.push((0x00, vec![], 0));
    let mut t2 = vec![(0x20u8, vec![0u8], 48u32)];
    t2.extend(note(72, 48, 144));
    t2.extend(note(72, 48, 96));
    t2.push((0x00, vec![], 0));
    bgm(0, &[(0, vec![(0x08, vec![120], 384), (0x00, vec![], 0)]), (0, t1), (0, t2)])
}

fn render(s: &Session, seconds: f64) -> Vec<[i16; 2]> {
    let mut p = Player::new(tables(), s.bgm().unwrap(), s.wd.clone(), RATE);
    p.raw = true;
    p.set_settings(s.settings());
    p.restart();
    let mut out = Vec::new();
    p.render_i16((seconds * f64::from(RATE)) as usize, &mut out);
    out
}

fn session() -> Session {
    Session::new(0, "test".into(), &song(), wd()).unwrap()
}

fn energy(x: &[[i16; 2]]) -> f64 {
    x.iter().map(|s| f64::from(s[0]).powi(2) + f64::from(s[1]).powi(2)).sum::<f64>() / x.len().max(1) as f64
}

fn crossings(x: &[[i16; 2]]) -> usize {
    x.windows(2).filter(|w| (w[0][0] >= 0) != (w[1][0] >= 0)).count()
}

fn peak(x: &[[i16; 2]]) -> i32 {
    x.iter().map(|s| i32::from(s[0]).abs().max(i32::from(s[1]).abs())).max().unwrap_or(0)
}

/// Frames of seconds `a..b`.
fn span(x: &[[i16; 2]], a: f64, b: f64) -> &[[i16; 2]] {
    &x[(a * 48000.0) as usize..(b * 48000.0) as usize]
}

#[test]
fn round_trip_synthetic() {
    let orig = Arc::new(Bgm::parse(&song()).unwrap());
    let mut p = Player::new(tables(), orig, wd(), RATE);
    p.raw = true;
    let mut want = Vec::new();
    p.render_i16(48000 * 6, &mut want);
    assert!(energy(&want) > 1e5, "the song sounds");
    let mut s = session();
    assert_eq!(render(&s, 6.0), want, "decoded and encoded");
    let f = s.seq.flags();
    for t in &mut s.seq.tracks {
        t.normalize(f);
    }
    assert_eq!(render(&s, 6.0), want, "normalized");
}

#[test]
fn mute_and_solo() {
    let base = render(&session(), 2.0);
    let mut s = session();
    s.channels[1].mute = true;
    s.channels[2].mute = true;
    assert_eq!(peak(&render(&s, 2.0)), 0, "both note tracks muted");
    let mut s = session();
    s.channels[2].solo = true;
    let solo = render(&s, 2.0);
    // track 1 plays alone in 0..0.5 s: silent when track 2 is soloed
    assert_eq!(peak(span(&solo, 0.01, 0.49)), 0);
    assert!(peak(span(&base, 0.01, 0.49)) > 1000);
    assert!(peak(span(&solo, 0.51, 0.99)) > 1000, "track 2's note");
}

#[test]
fn transpose_doubles_the_pitch() {
    let base = render(&session(), 0.45);
    let mut s = session();
    s.channels[1].transpose = 12;
    let up = render(&s, 0.45);
    let (a, b) = (crossings(&base), crossings(&up));
    assert!((b as f64 / a as f64 - 2.0).abs() < 0.05, "{a} -> {b} zero crossings");
}

#[test]
fn tempo_moves_the_notes() {
    // the second note of track 1 starts at 1 s; at tempo x2 at 0.5 s
    let mut s = session();
    s.channels[2].mute = true;
    let base = render(&s, 1.2);
    s.tempo = 2.0;
    let fast = render(&s, 1.2);
    assert_eq!(peak(span(&base, 0.6, 0.95)), 0);
    assert!(peak(span(&base, 1.01, 1.2)) > 1000);
    assert!(peak(span(&fast, 0.51, 0.7)) > 1000);
}

#[test]
fn note_edits() {
    let mut s = session();
    s.channels[2].mute = true;
    // add a note in the gap 0.5..1 s (ticks 48..96)
    s.seq.add_note(1, NoteSpec { start: 60, len: 20, key: 60, vel: 100 });
    let added = render(&s, 1.0);
    assert!(peak(span(&added, 0.66, 0.8)) > 1000, "the added note sounds");
    // delete the first note
    let first = s.seq.notes(1).into_iter().find(|n| n.start == 0).unwrap();
    s.seq.set_note(1, first.id, None);
    let deleted = render(&s, 1.0);
    assert_eq!(peak(span(&deleted, 0.0, 0.6)), 0, "the first note is gone");
    // half the velocity, about half the level
    let base = render(&session(), 0.4);
    let mut s = session();
    let n = s.seq.notes(1)[0];
    s.seq.set_note(1, n.id, Some(NoteSpec { start: n.start, len: n.len, key: n.key, vel: 50 }));
    let soft = render(&s, 0.4);
    let r = f64::from(peak(&soft)) / f64::from(peak(&base));
    assert!((r - 51.0 / 101.0).abs() < 0.05, "level ratio {r}");
    // move a note an octave up and later
    let mut s = session();
    s.channels[2].mute = true;
    s.seq.set_note(1, n.id, Some(NoteSpec { start: 24, len: 20, key: 72, vel: 100 }));
    let moved = render(&s, 0.5);
    assert_eq!(peak(span(&moved, 0.0, 0.24)), 0);
    let c = crossings(span(&moved, 0.26, 0.45));
    let c0 = crossings(span(&base, 0.06, 0.25));
    assert!((c as f64 / c0 as f64 - 2.0).abs() < 0.1, "{c0} -> {c}");
}

#[test]
fn the_song_loops_at_its_own_points() {
    let s = session();
    assert_eq!(s.song_loop(), (Some(96), 384));
    let mut p = Player::new(tables(), s.bgm().unwrap(), s.wd.clone(), RATE);
    let mut out = Vec::new();
    p.render_i16(48000 * 10, &mut out);
    let j = &p.song().marks.loop_jumps;
    // the loop is 384 - 96 ticks = 3 s
    assert!(j.len() >= 2);
    assert!((j[1] as i64 - j[0] as i64 - 144_000).abs() <= 200, "{j:?}");
}

#[test]
fn loop_markers() {
    let mut s = session();
    s.set_loop(Some((96, 192)));
    let mut p = Player::new(tables(), s.bgm().unwrap(), s.wd.clone(), RATE);
    p.set_settings(s.settings());
    p.restart();
    let mut out = Vec::new();
    p.render_i16(48000 * 6, &mut out);
    let j = p.song().marks.loop_jumps.clone();
    // first jump at tick 192 (2 s), then every 96 ticks (1 s)
    assert!((j[0] as i64 - 96_000).abs() <= 400, "{j:?}");
    for w in j.windows(2) {
        assert!((w[1] as i64 - w[0] as i64 - 48_000).abs() <= 200, "{j:?}");
    }
    // the song's own loop (3 s) does not happen
    assert!(j.len() >= 4);
    // what plays after a jump is what played at tick 96
    // (inside the note: its onset is on an interrupt, 4 ms steps)
    let a = span(&out, 1.05, 1.4);
    let t = j[1] as f64 / 48000.0;
    let b = span(&out, t + 0.05, t + 0.4);
    assert!(crossings(a).abs_diff(crossings(b)) <= 2, "{} {}", crossings(a), crossings(b));
    assert!((energy(a) / energy(b) - 1.0).abs() < 0.1);

    // no loop: the song ends
    let mut s = session();
    s.looping = false;
    let mut p = Player::new(tables(), s.bgm().unwrap(), s.wd.clone(), RATE);
    p.set_settings(s.settings());
    p.restart();
    let mut out = Vec::new();
    p.render_i16(48000 * 6, &mut out);
    assert!(p.song().marks.loop_jumps.is_empty());
    assert!(p.finished());
    assert_eq!(peak(span(&out, 5.0, 6.0)), 0);
}

/// A region with flag 0x10 (banks 143 and 149 on the disc): one long sample
/// in 0x2000-byte pieces, each ending in loop end + repeat; the driver moves
/// the loop start on piece by piece, so the sample plays through once.
#[test]
fn a_long_sample_in_pieces_plays_through() {
    let mut b = vec![0u8; 0x20];
    b[..2].copy_from_slice(b"WD");
    b[4..8].copy_from_slice(&(3 * 0x2000u32).to_le_bytes());
    b[8..12].copy_from_slice(&1u32.to_le_bytes());
    b[12..16].copy_from_slice(&1u32.to_le_bytes());
    b.extend_from_slice(&0x30u32.to_le_bytes());
    b.resize(0x30, 0);
    let mut r = vec![0u8; 0x20];
    r[0..4].copy_from_slice(&0x310u32.to_le_bytes());
    r[8..12].copy_from_slice(&0x2000u32.to_le_bytes());
    r[12..16].copy_from_slice(&0x1fc0_000fu32.to_le_bytes());
    r[0x14] = 127;
    r[0x15] = 127;
    r[0x16] = 127;
    b.extend(r);
    // pieces: square wave, silence, square wave (0.3 s each at key 60)
    for piece in 0..3 {
        for k in 0..512 {
            let flags = if k < 511 { 0 } else if piece < 2 { 3 } else { 1 };
            let mut blk = vec![0x00u8, flags];
            let (hi, lo) = if piece == 1 { (0, 0) } else { (0x77, 0x99) };
            blk.extend([hi; 7]);
            blk.extend([lo; 7]);
            b.extend(blk);
        }
    }
    let wd = Arc::new(Wd::parse(&b).expect("synthetic WD"));
    let t1 = vec![(0x20u8, vec![0u8], 0u32), (0x11, vec![60, 127], 192), (0x1a, vec![60], 0), (0x00, vec![], 0)];
    let song = bgm(0, &[(0, vec![(0x08, vec![120], 192), (0x00, vec![], 0)]), (0, t1)]);
    let s = Session::new(0, "pieces".into(), &song, wd).unwrap();
    let out = render(&s, 1.5);
    assert!(peak(span(&out, 0.05, 0.25)) > 1000, "piece 0");
    assert_eq!(peak(span(&out, 0.36, 0.55)), 0, "piece 1 is silent");
    assert!(peak(span(&out, 0.66, 0.85)) > 1000, "piece 2 (looping piece 1 before the fix)");
    assert_eq!(peak(span(&out, 1.0, 1.5)), 0, "the sample ended");
}

#[test]
fn seek_lands_on_the_tick() {
    let s = session();
    let mut p = Player::new(tables(), s.bgm().unwrap(), s.wd.clone(), RATE);
    p.seek_tick(200);
    assert!(p.song().tick >= 200 && p.song().tick < 203);
    p.seek_tick(10);
    assert!(p.song().tick >= 10 && p.song().tick < 13);
}

#[test]
fn instrument_from_another_bank() {
    let mut s = session();
    let other = wd();
    s.set_instrument(1, Some((7, &other, 0)));
    assert_eq!(s.wd.instruments.len(), 2);
    assert_eq!(s.channels[1].instrument, Some((7, 0, 1)));
    // the same sound (it is the same instrument), from the appended copy
    assert_eq!(render(&s, 1.0), render(&session(), 1.0));
}

/// The disc's tracks, when `FFX_STUDIO_CONFIG` names a demaster.toml.
fn disc() -> Option<crate::library::Library> {
    let Some(c) = std::env::var_os("FFX_STUDIO_CONFIG") else {
        eprintln!("FFX_STUDIO_CONFIG not set: disc tests skipped");
        return None;
    };
    Some(crate::library::Library::open(Some(std::path::Path::new(&c))).expect("disc"))
}

#[test]
fn disc_sequences_encode_to_the_same_bytes() {
    let Some(mut lib) = disc() else { return };
    let ids: Vec<u16> = lib.tracks.iter().map(|t| t.id).collect();
    for id in ids {
        let (b, _, _) = lib.song(id).unwrap();
        let s = Seq::decode(&b).unwrap();
        assert_eq!(s.encode(), b, "music{id:03}");
        let mut n = s.clone();
        let f = s.flags();
        for t in &mut n.tracks {
            t.normalize(f);
        }
        for ti in 0..s.tracks.len() {
            let (a, b) = (s.notes(ti), n.notes(ti));
            assert_eq!(a.len(), b.len(), "music{id:03} track {ti}");
            for (x, y) in a.iter().zip(&b) {
                assert_eq!((x.start, x.len, x.key, x.vel), (y.start, y.len, y.key, y.vel), "music{id:03} track {ti}");
            }
        }
    }
}

/// Renders `seconds` of a disc track three ways: the disc's bytes, decoded
/// and encoded, normalized; all must be the same samples.
fn disc_round_trip(lib: &mut crate::library::Library, id: u16, seconds: f64) {
    let (b, wd, missing) = lib.song(id).unwrap();
    if let Some(m) = missing {
        eprintln!("{m}");
        return;
    }
    let n = (seconds * 48000.0) as usize;
    let play = |bgm: Arc<Bgm>| {
        let mut p = Player::new(lib.tables.clone(), bgm, wd.clone(), RATE);
        p.raw = true;
        let mut out = Vec::new();
        p.render_i16(n, &mut out);
        out
    };
    let want = play(Arc::new(Bgm::parse(&b).unwrap()));
    let mut s = Seq::decode(&b).unwrap();
    assert!(play(Arc::new(s.to_bgm().unwrap())) == want, "music{id:03} decoded");
    let f = s.flags();
    for t in &mut s.tracks {
        t.normalize(f);
    }
    assert!(play(Arc::new(s.to_bgm().unwrap())) == want, "music{id:03} normalized");
}

#[test]
fn disc_round_trip_renders_the_same() {
    let Some(mut lib) = disc() else { return };
    // battle theme, Zanarkand, game over, prelude, the one with auto key-off
    for id in [16, 130, 140, 48, 240] {
        if lib.tracks.iter().any(|t| t.id == id) {
            disc_round_trip(&mut lib, id, 8.0);
        }
    }
}

/// All tracks, 30 s each (`cargo test --release -- --ignored`).
#[test]
#[ignore]
fn disc_round_trip_all_tracks() {
    let Some(mut lib) = disc() else { return };
    let ids: Vec<u16> = lib.tracks.iter().map(|t| t.id).collect();
    for id in ids {
        disc_round_trip(&mut lib, id, 30.0);
    }
}

/// RMS of frames `a..b` (s).
fn rms(x: &[[i16; 2]], a: f64, b: f64) -> f64 {
    energy(span(x, a, b)).sqrt()
}

#[test]
fn pause_lets_the_notes_fade_out() {
    // track 1's first note sounds 0..0.5 s; pause in the middle of it
    let mut s = session();
    s.wd = wd_release(0x0c);
    s.channels[2].mute = true;
    let mut p = Player::new(tables(), s.bgm().unwrap(), s.wd.clone(), RATE);
    p.raw = true;
    p.set_settings(s.settings());
    p.restart();
    let mut a = Vec::new();
    p.render_i16(12_000, &mut a);
    let tick = p.song().tick;
    let before = rms(&a, 0.2, 0.25);
    assert!(before > 1000.0, "the note sounds: {before}");
    p.pause();
    assert!(p.fading());
    let mut b = Vec::new();
    p.render_i16(48_000, &mut b);
    // no step to zero: the first 5 ms after the pause are as loud as before
    let first = rms(&b, 0.0, 0.005);
    assert!(first > 0.6 * before, "{first} after {before}");
    // it decays: each 20 ms quieter than the one before, until silent
    let w: Vec<f64> = (0..10).map(|i| rms(&b, i as f64 * 0.02, (i + 1) as f64 * 0.02)).collect();
    for x in w.windows(2) {
        assert!(x[1] <= x[0] * 1.05 + 1.0, "{w:?}");
    }
    assert!(w[2] < 0.8 * w[0] && w[2] > 0.0, "{w:?}");
    // and reaches silence, the sequence standing still
    assert_eq!(peak(span(&b, 0.5, 1.0)), 0);
    assert!(!p.fading(), "the fade ends");
    assert_eq!(p.song().tick, tick, "the sequence stays at the pause");
    // resume: on from the same tick, the next note at 1 s of song time
    p.resume();
    let mut c = Vec::new();
    p.render_i16(48_000, &mut c);
    assert!(p.song().tick > tick + 60);
    assert!(peak(span(&c, 0.8, 1.0)) > 1000, "the next note plays");
}

#[test]
fn stop_fades_a_copy_and_rewinds() {
    let mut s = session();
    s.wd = wd_release(0x0c);
    let mut p = Player::new(tables(), s.bgm().unwrap(), s.wd.clone(), RATE);
    p.raw = true;
    let mut a = Vec::new();
    p.render_i16(12_000, &mut a);
    let mut tail = p.stop_with_tail();
    assert_eq!(p.song().tick, 0);
    let mut b = Vec::new();
    tail.render_i16(48_000, &mut b);
    assert!(rms(&b, 0.0, 0.005) > 0.6 * rms(&a, 0.2, 0.25));
    assert_eq!(peak(span(&b, 0.5, 1.0)), 0);
    assert!(!tail.fading());
}

#[test]
fn channel_levels_follow_the_voices() {
    let s = session();
    let mut p = Player::new(tables(), s.bgm().unwrap(), s.wd.clone(), RATE);
    let mut out = Vec::new();
    p.render(9600, &mut out);
    let l = p.take_levels();
    assert_eq!(l.len(), 3);
    // track 1 plays from tick 0, track 2 from tick 48 (0.5 s)
    assert!(l[1] > 0.05, "{l:?}");
    assert_eq!((l[0], l[2]), (0.0, 0.0));
    // taken: back to 0 until more is played
    assert!(p.take_levels().iter().all(|&x| x == 0.0));
    p.render(24_000, &mut out);
    assert!(p.take_levels()[2] > 0.05);
}

#[test]
fn live_notes_on_a_channel() {
    let w = wd_release(0x0c);
    let ram = Arc::new(w.samples.clone());
    let mut live = Live::new(tables(), w.clone(), ram, 3, RATE);
    assert!(!live.active());
    let gain = 1.0 / 32768.0;
    let m = TrackMod { program: Some(0), ..TrackMod::default() };
    live.note_on(1, 60, 127, m);
    assert!(live.active());
    let mut out = vec![[0f32; 2]; 9600];
    live.mix(&mut out, gain);
    let loud = out[4800..].iter().map(|s| s[0].abs()).fold(0.0, f32::max);
    assert!(loud > 0.05, "the note sounds: {loud}");
    let l = live.take_levels();
    assert!(l[1] > 0.05 && l[0] == 0.0, "{l:?}");
    // half the velocity, about half the level
    let mut soft = Live::new(tables(), w.clone(), Arc::new(w.samples.clone()), 3, RATE);
    soft.note_on(1, 60, 63, m);
    let mut o2 = vec![[0f32; 2]; 9600];
    soft.mix(&mut o2, gain);
    let q = o2[4800..].iter().map(|s| s[0].abs()).fold(0.0, f32::max);
    assert!((q / loud - 0.5).abs() < 0.06, "{q} / {loud}");
    // key-off: the release, then silence and idle
    live.note_off(1, 60);
    let mut out = vec![[0f32; 2]; 48_000];
    live.mix(&mut out, gain);
    assert!(out[..240].iter().any(|s| s[0].abs() > 0.02), "the release starts loud");
    assert!(out[24_000..].iter().all(|s| s[0] == 0.0));
    assert!(!live.active());
    // the channel's transpose: an octave up doubles the zero crossings
    let mut a = Live::new(tables(), w.clone(), Arc::new(w.samples.clone()), 3, RATE);
    let mut b = Live::new(tables(), w.clone(), Arc::new(w.samples.clone()), 3, RATE);
    a.note_on(2, 60, 100, m);
    b.note_on(2, 60, 100, TrackMod { transpose: 12, ..m });
    let (mut x, mut y) = (vec![[0f32; 2]; 9600], vec![[0f32; 2]; 9600]);
    a.mix(&mut x, 1.0);
    b.mix(&mut y, 1.0);
    let zc = |v: &[[f32; 2]]| v.windows(2).filter(|w| (w[0][0] >= 0.0) != (w[1][0] >= 0.0)).count();
    let r = zc(&y[2400..]) as f64 / zc(&x[2400..]) as f64;
    assert!((r - 2.0).abs() < 0.1, "{r}");
}

#[test]
fn metronome_clicks_on_the_beats() {
    let mut s = session();
    s.channels[1].mute = true;
    s.channels[2].mute = true;
    let mut p = Player::new(tables(), s.bgm().unwrap(), s.wd.clone(), RATE);
    p.set_settings(s.settings());
    p.restart();
    p.metronome = true;
    let mut out = Vec::new();
    p.render_i16(48_000 * 2, &mut out);
    // 120 BPM: a click every 0.5 s, silence in between
    for k in 1..4 {
        let t = k as f64 * 0.5;
        assert!(peak(span(&out, t - 0.02, t + 0.03)) > 2000, "click at {t}");
        assert_eq!(peak(span(&out, t + 0.06, t + 0.4)), 0, "quiet after {t}");
    }
    p.metronome = false;
    let mut out = Vec::new();
    p.render_i16(48_000, &mut out);
    // (a click under way finishes)
    assert_eq!(peak(span(&out, 0.05, 1.0)), 0);
}

#[test]
fn export_writes_the_track_once() {
    let s = session();
    let path = std::env::temp_dir().join(format!("ffx-studio-export-{}.wav", std::process::id()));
    let secs = crate::player::export_wav(tables(), s.bgm().unwrap(), s.wd.clone(), s.settings(), &path, 60.0).unwrap();
    // the song is 4 s (it loops, the export does not), then 2 s of tail
    assert!((5.9..7.0).contains(&secs), "{secs}");
    let len = std::fs::metadata(&path).unwrap().len();
    assert!((len as f64 - (44.0 + secs * 48000.0 * 4.0)).abs() < 100.0, "{len}");
    std::fs::remove_file(&path).ok();
}

#[test]
fn midi_export_follows_the_driver() {
    use crate::export::smf;
    let b = Arc::new(Bgm::parse(&song()).unwrap());
    let log = smf::run_log(tables(), b.clone(), wd(), 100_000);
    let (bytes, info) = smf::build(&b, 3, &log, "test", &|p| format!("#{p}"));
    // the loop point at tick 96, the jump back at the end (tick 384)
    assert_eq!((info.loop_start, info.loop_end), (Some(96), Some(384)));
    assert_eq!(info.tempos, vec![(0, 120)]);
    assert_eq!(info.tracks[1].notes, 4);
    assert_eq!(info.tracks[2].notes, 2);
    assert!(info.timing_dev_ms < 5.0, "{}", info.timing_dev_ms);
    // header: type 1, conductor + 3 tracks, 48 ticks a quarter
    assert_eq!(&bytes[..4], b"MThd");
    assert_eq!(&bytes[8..14], &[0, 1, 0, 4, 0, 48]);
    // track 1: bank select 3, program 0, note 60 at ticks 0, 96, 192, 288 for 48
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("loopStart") && text.contains("loopEnd"));
    // past the conductor and sequence track 0 (the tempo)
    let mut p = 14;
    for _ in 0..2 {
        p += 8 + u32::from_be_bytes(bytes[p + 4..p + 8].try_into().unwrap()) as usize;
    }
    let len = u32::from_be_bytes(bytes[p + 4..p + 8].try_into().unwrap()) as usize;
    let d = &bytes[p + 8..p + 8 + len];
    p = 0;
    let (mut t, mut ons, mut offs, mut status) = (0u32, Vec::new(), Vec::new(), 0u8);
    let mut banks = Vec::new();
    while p < d.len() {
        let mut v = 0u32;
        loop {
            let b = d[p];
            p += 1;
            v = v << 7 | u32::from(b & 0x7f);
            if b & 0x80 == 0 {
                break;
            }
        }
        t += v;
        if d[p] == 0xff {
            let n = d[p + 2] as usize;
            p += 3 + n;
            continue;
        }
        status = d[p];
        let (a, b2) = (d[p + 1], d.get(p + 2).copied().unwrap_or(0));
        match status & 0xf0 {
            0x90 => ons.push((t, a, b2)),
            0x80 => offs.push((t, a)),
            0xb0 if a == 0 => banks.push(b2),
            _ => {}
        }
        p += if status & 0xf0 == 0xc0 { 2 } else { 3 };
    }
    let _ = status;
    assert_eq!(ons, vec![(0, 60, 100), (96, 60, 100), (192, 60, 100), (288, 60, 100)]);
    assert_eq!(offs, vec![(48, 60), (144, 60), (240, 60), (336, 60)]);
    assert_eq!(banks, vec![3]);
}

#[test]
fn soundfont_bank_of_the_synthetic_wd() {
    use crate::export::sf2;
    let w = wd();
    let b = sf2::bank(&w, 0);
    assert_eq!(b.samples.len(), 1);
    // one block that loops on itself
    assert_eq!(b.samples[0].looped, Some((0, 28)));
    assert_eq!(b.samples[0].pcm.len(), 28);
    let z = &b.instruments[0].zones;
    assert_eq!(z.len(), 1);
    assert_eq!((z[0].keys, z[0].vels), ((0, 127), (0, 127)));
    // tune 0: key 60 plays the sample at 48 kHz
    assert_eq!((b.samples[0].rate, b.samples[0].root, z[0].tune_cents), (48000, 60, 0));
    // instant attack, sustain at full level
    assert_eq!((z[0].env.attack, z[0].env.sustain), (-12000, 0));
    let path = std::env::temp_dir().join(format!("ffx-studio-{}.sf2", std::process::id()));
    sf2::write(&path, "t", &[&b]).unwrap();
    let f = std::fs::read(&path).unwrap();
    assert_eq!((&f[..4], &f[8..12]), (&b"RIFF"[..], &b"sfbk"[..]));
    assert_eq!(u32::from_le_bytes(f[4..8].try_into().unwrap()) as usize, f.len() - 8);
    std::fs::remove_file(&path).ok();
}
