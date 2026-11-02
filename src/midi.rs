//! A MIDI keyboard (`midir`; Windows, and Linux with `--features alsa`):
//! the messages become live notes on the selected channel's instrument
//! (velocity, sustain pedal CC64, pitch bend), sent straight from the MIDI
//! thread to the audio engine. The window reads what was played (for the
//! keys it lights and the recording).
// without a MIDI backend (Linux without `alsa`) only the tests parse messages
#![cfg_attr(not(any(windows, feature = "alsa")), allow(dead_code))]

use std::collections::BTreeSet;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use demaster::sqseq::TrackMod;

use crate::audio::{Cmd, LiveCmd};

/// A MIDI message the studio uses (any MIDI channel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Msg {
    On { key: u8, vel: u8 },
    Off { key: u8 },
    Sustain(bool),
    /// -0x2000..0x1fff
    Bend(i32),
    /// All notes off / all sound off (CC 120, 123).
    AllOff,
}

pub fn parse(b: &[u8]) -> Option<Msg> {
    let status = *b.first()?;
    let d = |i: usize| b.get(i).copied().filter(|x| x & 0x80 == 0);
    match status & 0xf0 {
        0x90 => {
            let (key, vel) = (d(1)?, d(2)?);
            Some(if vel == 0 { Msg::Off { key } } else { Msg::On { key, vel } })
        }
        0x80 => Some(Msg::Off { key: d(1)? }),
        0xb0 => match (d(1)?, d(2)?) {
            (64, v) => Some(Msg::Sustain(v >= 64)),
            (120 | 123, _) => Some(Msg::AllOff),
            _ => None,
        },
        0xe0 => Some(Msg::Bend((i32::from(d(1)?) | i32::from(d(2)?) << 7) - 0x2000)),
        _ => None,
    }
}

/// Where played notes go: the channel and its sound.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Target {
    pub ch: usize,
    pub m: TrackMod,
}

/// A note played, for the window (recording, the lit keys).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Played {
    On { ch: usize, key: u8, vel: u8 },
    Off { ch: usize, key: u8 },
}

/// The keyboard's state: the pedal, the keys down, the notes sounding and
/// the channel each one plays on.
#[derive(Debug, Clone)]
pub struct Keyboard {
    pub target: Option<Target>,
    pedal: bool,
    /// Keys let go while the pedal is down (still sounding).
    held: BTreeSet<u8>,
    /// The channel each sounding key plays on.
    on: [Option<usize>; 128],
    /// What happened since the window last looked.
    pub played: Vec<Played>,
}

impl Default for Keyboard {
    fn default() -> Self {
        Keyboard { target: None, pedal: false, held: BTreeSet::new(), on: [None; 128], played: Vec::new() }
    }
}

impl Keyboard {
    /// A message in, the engine's commands out.
    pub fn feed(&mut self, m: Msg) -> Vec<LiveCmd> {
        let mut out = Vec::new();
        match m {
            Msg::On { key, vel } => {
                let Some(t) = self.target else { return out };
                let k = usize::from(key & 0x7f);
                if let Some(ch) = self.on[k].take() {
                    // struck again while it sounds (the pedal holds it)
                    out.push(LiveCmd::Off { ch, key });
                    self.played.push(Played::Off { ch, key });
                }
                self.held.remove(&key);
                self.on[k] = Some(t.ch);
                out.push(LiveCmd::On { ch: t.ch, key, vel, m: t.m });
                self.played.push(Played::On { ch: t.ch, key, vel });
            }
            Msg::Off { key } => {
                if self.pedal {
                    if self.on[usize::from(key & 0x7f)].is_some() {
                        self.held.insert(key);
                    }
                } else {
                    self.release(key, &mut out);
                }
            }
            Msg::Sustain(down) => {
                self.pedal = down;
                if !down {
                    for key in std::mem::take(&mut self.held) {
                        self.release(key, &mut out);
                    }
                }
            }
            Msg::Bend(raw) => {
                if let Some(t) = self.target {
                    out.push(LiveCmd::Bend { ch: t.ch, raw });
                }
            }
            Msg::AllOff => {
                self.held.clear();
                for key in 0..128u8 {
                    self.release(key, &mut out);
                }
            }
        }
        out
    }

    fn release(&mut self, key: u8, out: &mut Vec<LiveCmd>) {
        if let Some(ch) = self.on[usize::from(key & 0x7f)].take() {
            out.push(LiveCmd::Off { ch, key });
            self.played.push(Played::Off { ch, key });
        }
    }

    /// The keys sounding on channel `ch`.
    pub fn sounding(&self, ch: usize) -> impl Iterator<Item = u8> + '_ {
        (0..128u8).filter(move |&k| self.on[usize::from(k)] == Some(ch))
    }
}

/// An open MIDI input.
pub struct Input {
    pub name: String,
    #[cfg(any(windows, feature = "alsa"))]
    _conn: midir::MidiInputConnection<()>,
}

/// The MIDI input devices (names), or why there are none.
#[cfg(any(windows, feature = "alsa"))]
pub fn ports() -> Result<Vec<String>, String> {
    let m = midir::MidiInput::new("ffx-music-studio").map_err(|e| e.to_string())?;
    Ok(m.ports().iter().filter_map(|p| m.port_name(p).ok()).collect())
}

#[cfg(not(any(windows, feature = "alsa")))]
pub fn ports() -> Result<Vec<String>, String> {
    Err("MIDI input needs the Windows build (or --features alsa on Linux)".into())
}

/// Opens the input named `name`: its messages go through `kb` to the
/// engine (`tx`); `repaint` wakes the window.
#[cfg(any(windows, feature = "alsa"))]
pub fn open(name: &str, kb: Arc<Mutex<Keyboard>>, tx: Sender<Cmd>, repaint: Box<dyn Fn() + Send>) -> Result<Input, String> {
    let m = midir::MidiInput::new("ffx-music-studio").map_err(|e| e.to_string())?;
    let port = m.ports().into_iter().find(|p| m.port_name(p).ok().as_deref() == Some(name)).ok_or_else(|| format!("no MIDI input {name}"))?;
    let conn = m
        .connect(
            &port,
            "ffx-music-studio-in",
            move |_, msg, _| {
                let Some(m) = parse(msg) else { return };
                if let Ok(mut k) = kb.lock() {
                    for c in k.feed(m) {
                        let _ = tx.send(Cmd::Live(c));
                    }
                }
                repaint();
            },
            (),
        )
        .map_err(|e| e.to_string())?;
    Ok(Input { name: name.to_string(), _conn: conn })
}

#[cfg(not(any(windows, feature = "alsa")))]
pub fn open(_name: &str, _kb: Arc<Mutex<Keyboard>>, _tx: Sender<Cmd>, _repaint: Box<dyn Fn() + Send>) -> Result<Input, String> {
    Err("MIDI input needs the Windows build (or --features alsa on Linux)".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages() {
        assert_eq!(parse(&[0x90, 60, 100]), Some(Msg::On { key: 60, vel: 100 }));
        assert_eq!(parse(&[0x93, 60, 0]), Some(Msg::Off { key: 60 }));
        assert_eq!(parse(&[0x80, 61, 64]), Some(Msg::Off { key: 61 }));
        assert_eq!(parse(&[0xb0, 64, 127]), Some(Msg::Sustain(true)));
        assert_eq!(parse(&[0xb0, 64, 0]), Some(Msg::Sustain(false)));
        assert_eq!(parse(&[0xb0, 123, 0]), Some(Msg::AllOff));
        assert_eq!(parse(&[0xb0, 7, 100]), None);
        assert_eq!(parse(&[0xe0, 0, 0x40]), Some(Msg::Bend(0)));
        assert_eq!(parse(&[0xe0, 0x7f, 0x7f]), Some(Msg::Bend(0x1fff)));
        assert_eq!(parse(&[0xe0, 0, 0]), Some(Msg::Bend(-0x2000)));
        assert_eq!(parse(&[0xf8]), None);
        assert_eq!(parse(&[0x90, 60]), None);
        assert_eq!(parse(&[]), None);
    }

    fn kb() -> Keyboard {
        Keyboard { target: Some(Target { ch: 3, m: TrackMod { program: Some(5), ..TrackMod::default() } }), ..Keyboard::default() }
    }

    fn feed(k: &mut Keyboard, b: &[u8]) -> Vec<LiveCmd> {
        k.feed(parse(b).unwrap())
    }

    #[test]
    fn notes_with_velocity_on_the_channel() {
        let mut k = kb();
        let m = k.target.unwrap().m;
        assert_eq!(feed(&mut k, &[0x90, 60, 90]), vec![LiveCmd::On { ch: 3, key: 60, vel: 90, m }]);
        assert_eq!(k.sounding(3).collect::<Vec<_>>(), vec![60]);
        // the channel changes while the key is down: its key-off goes where
        // the note is
        k.target = Some(Target { ch: 4, m });
        assert_eq!(feed(&mut k, &[0x80, 60, 0]), vec![LiveCmd::Off { ch: 3, key: 60 }]);
        assert_eq!(k.played, vec![Played::On { ch: 3, key: 60, vel: 90 }, Played::Off { ch: 3, key: 60 }]);
        // no channel: nothing plays
        k.target = None;
        assert!(feed(&mut k, &[0x90, 62, 90]).is_empty());
    }

    #[test]
    fn the_sustain_pedal_holds_notes() {
        let mut k = kb();
        feed(&mut k, &[0xb0, 64, 127]);
        feed(&mut k, &[0x90, 60, 90]);
        feed(&mut k, &[0x90, 64, 90]);
        assert!(feed(&mut k, &[0x80, 60, 0]).is_empty());
        assert!(feed(&mut k, &[0x90, 64, 0]).is_empty());
        // struck again under the pedal: off, then on again
        let r = feed(&mut k, &[0x90, 60, 70]);
        assert!(matches!(r[..], [LiveCmd::Off { ch: 3, key: 60 }, LiveCmd::On { ch: 3, key: 60, vel: 70, .. }]));
        // pedal up: the keys let go end (60 is down again: it stays)
        let r = feed(&mut k, &[0xb0, 64, 0]);
        assert_eq!(r, vec![LiveCmd::Off { ch: 3, key: 64 }]);
        assert_eq!(feed(&mut k, &[0x80, 60, 0]), vec![LiveCmd::Off { ch: 3, key: 60 }]);
    }

    #[test]
    fn bend_and_all_off() {
        let mut k = kb();
        assert_eq!(feed(&mut k, &[0xe1, 0, 0x60]), vec![LiveCmd::Bend { ch: 3, raw: 0x1000 }]);
        feed(&mut k, &[0x90, 60, 90]);
        feed(&mut k, &[0x90, 67, 90]);
        let r = feed(&mut k, &[0xb0, 123, 0]);
        assert_eq!(r, vec![LiveCmd::Off { ch: 3, key: 60 }, LiveCmd::Off { ch: 3, key: 67 }]);
        assert!(k.sounding(3).next().is_none());
    }
}
