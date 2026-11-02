//! Tests of the playback path: the round trip (decoded and re-encoded
//! sequences play exactly as the original), edits change the output as
//! they should, the loop markers. The synthetic ones need no disc; the disc
//! ones run when `FFX_STUDIO_CONFIG` names a `demaster.toml` (else they say
//! so and pass).

use std::sync::Arc;

use demaster::spu2::RATE;
use demaster::sqseq::{Bgm, Tables, Wd};

use crate::player::Player;
use crate::seq::tests::bgm;
use crate::seq::{NoteSpec, Seq};
use crate::session::Session;

/// A bank of one instrument: a square wave of 28 samples (1714 Hz at key
/// 60), instant attack, full sustain, fast release.
fn wd() -> Arc<Wd> {
    let mut b = vec![0u8; 0x20];
    b[..2].copy_from_slice(b"WD");
    b[4..8].copy_from_slice(&16u32.to_le_bytes()); // sample bytes
    b[8..12].copy_from_slice(&1u32.to_le_bytes()); // instruments
    b[12..16].copy_from_slice(&1u32.to_le_bytes()); // regions
    b.extend_from_slice(&0x30u32.to_le_bytes()); // first region
    b.resize(0x30, 0);
    let mut r = vec![0u8; 0x20];
    r[0..4].copy_from_slice(&0x300u32.to_le_bytes());
    r[12..16].copy_from_slice(&0x1fc0_000fu32.to_le_bytes());
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
