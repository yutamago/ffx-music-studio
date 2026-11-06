//! MIDI to driver commands. The MIDI files of `ffx-music-studio --export`
//! carry, next to the General MIDI controllers, the driver's own values as
//! controllers of their own (the table in `HOW-TO-OPEN.md`, "FFX PS2 Sound
//! controllers"). Everything that arrives for one tick and channel is
//! gathered first and turned into commands in a fixed order, so the order a
//! DAW plays events of one tick in does not matter: the header and global
//! values, the program, the envelope, the mix values, the pitch values, the
//! LFOs, the tie, then key-offs, key-ons and the end.

use demaster::sqseq::{EXT_EXPRESSION, EXT_MIX, EXT_PAN, EXT_SONG_END, EXT_VOLUME, ExtHeader};

/// Driver tracks: three MIDI ports ("parts") of 16 channels (the export
/// puts a song's 16th and later channels with notes on the next port).
pub const TRACKS: usize = 48;

/// A MIDI channel message; `ch` is the driver track: port * 16 + channel.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Midi {
    NoteOn { ch: u8, key: u8, vel: u8 },
    NoteOff { ch: u8, key: u8 },
    Cc { ch: u8, cc: u8, value: u8 },
    Program { ch: u8, program: u8 },
    /// 14-bit, 0x2000 centre.
    Bend { ch: u8, value: u16 },
}

impl Midi {
    pub fn channel(&self) -> u8 {
        match *self {
            Midi::NoteOn { ch, .. } | Midi::NoteOff { ch, .. } | Midi::Cc { ch, .. } | Midi::Program { ch, .. } | Midi::Bend { ch, .. } => ch,
        }
    }

    /// The same message on port `part` (its channel + 16 * part).
    pub fn on_part(mut self, part: u8) -> Midi {
        let add = part.saturating_mul(16);
        match &mut self {
            Midi::NoteOn { ch, .. } | Midi::NoteOff { ch, .. } | Midi::Cc { ch, .. } | Midi::Program { ch, .. } | Midi::Bend { ch, .. } => {
                *ch = ch.saturating_add(add)
            }
        }
        self
    }

    /// From raw bytes (status, data...); note-on with velocity 0 is a note-off.
    pub fn parse(b: &[u8]) -> Option<Midi> {
        let s = *b.first()?;
        let ch = s & 0xf;
        let d = |i: usize| b.get(i).copied().unwrap_or(0) & 0x7f;
        Some(match s & 0xf0 {
            0x80 => Midi::NoteOff { ch, key: d(1) },
            0x90 if d(2) == 0 => Midi::NoteOff { ch, key: d(1) },
            0x90 => Midi::NoteOn { ch, key: d(1), vel: d(2) },
            0xb0 => Midi::Cc { ch, cc: d(1), value: d(2) },
            0xc0 => Midi::Program { ch, program: d(1) },
            0xe0 => Midi::Bend { ch, value: u16::from(d(1)) | u16::from(d(2)) << 7 },
            _ => return None,
        })
    }
}

/// The controller numbers (HOW-TO-OPEN.md, "FFX PS2 Sound controllers").
pub mod cc {
    /// The note-off of this key in this tick is a re-strike: the old voice
    /// sounds on (the driver keys a sounding key on again without a key-off).
    pub const RESTRIKE: u8 = 3;
    /// Track end (`0x00`).
    pub const TRACK_END: u8 = 9;
    /// The note-on of this key in this tick has velocity 0 (the driver
    /// plays it at full volume, but picks the region by velocity).
    pub const VEL0: u8 = 13;
    /// This key is keyed on twice in this tick (two voices).
    pub const DOUBLE: u8 = 12;
    /// Pitch LFO delay and fade (`0x47`) that the driver set after the
    /// tick's LFO start (`0x40`): played after it.
    pub const PLFO_DELAY_AFTER: u8 = 57;
    pub const PLFO_FADE_AFTER: u8 = 58;
    /// Header reverb depth / 2 (14 bits with LSB 46).
    pub const HDR_DEPTH: u8 = 14;
    /// The song's end (no loop): every voice keyed off and let go.
    pub const SONG_END: u8 = 15;
    /// Volume, expression, pan (14 bits with LSB +32): the driver's values.
    pub const VOLUME: u8 = 20;
    pub const EXPRESSION: u8 = 21;
    /// Pan position + 0x1f80 (0x2000: centre 0x80).
    pub const PAN: u8 = 22;
    /// Tempo in BPM (14 bits).
    pub const TEMPO: u8 = 23;
    /// Reverb depth (`0x0a`), the byte (14 bits).
    pub const DEPTH: u8 = 24;
    /// The mix switches: bit 0/1 dry left/right, 2/3 wet left/right.
    pub const MIX: u8 = 25;
    /// Tie (`0x2a` at 64 and above, `0x2b` below).
    pub const TIE: u8 = 26;
    /// Portamento time in ticks (`0x28`; 0x3fff: off, `0x29`), 14 bits.
    pub const PORTA: u8 = 27;
    /// Pitch slide (`0x2c`): ticks (14 bits), then the target byte (14 bits)
    /// which starts it.
    pub const SLIDE_TIME: u8 = 28;
    pub const SLIDE_TO: u8 = 29;
    /// Detune (`0x5a`) and pitch scale (`0x58`), the byte (14 bits).
    pub const DETUNE: u8 = 30;
    pub const SCALE: u8 = 31;
    /// Voice flags `0x3e`, `0x3f` and the first voice (`0x3d`).
    pub const VOICE_FLAG_20: u8 = 85;
    pub const VOICE_FLAG_10: u8 = 86;
    pub const VOICE_START: u8 = 87;
    /// Header: song volume, reverb mode (10 and above: off), flags (64 and
    /// above: automatic key-offs, `0x10000`).
    pub const HDR_VOLUME: u8 = 88;
    pub const HDR_REVERB: u8 = 89;
    pub const HDR_FLAGS: u8 = 90;
    /// Envelope overrides `0x31`..`0x38` (102..109) and reset `0x30` (110).
    pub const ADSR: u8 = 102;
    pub const ADSR_RESET: u8 = 110;
    /// Pitch LFO: delay, fade (starts `0x47`), depth, rate, wave (starts
    /// `0x40`; 127: off, `0x41`).
    pub const PLFO_DELAY: u8 = 112;
    pub const PLFO_FADE: u8 = 113;
    pub const PLFO_DEPTH: u8 = 114;
    pub const PLFO_RATE: u8 = 115;
    pub const PLFO_WAVE: u8 = 116;
    /// Pan LFO: depth, rate, wave (starts `0x50`; 127: off, `0x51`).
    pub const PANLFO_DEPTH: u8 = 117;
    pub const PANLFO_RATE: u8 = 118;
    pub const PANLFO_WAVE: u8 = 119;
    /// "off" for the wave controllers.
    pub const LFO_OFF: u8 = 127;
}

/// Everything one channel got in one tick.
#[derive(Debug, Clone, Default)]
pub struct Batch {
    ccs: Vec<(u8, u8)>,
    program: Option<u8>,
    bend: Option<u16>,
    offs: Vec<u8>,
    ons: Vec<(u8, u8)>,
}

impl Batch {
    pub fn push(&mut self, m: Midi) {
        match m {
            Midi::NoteOn { key, vel, .. } => {
                if !self.ons.iter().any(|o| o.0 == key) {
                    self.ons.push((key, vel));
                }
            }
            Midi::NoteOff { key, .. } => {
                // offs come before ons, whatever order they arrived in (a
                // note of no length never comes from the export; a DAW may
                // play an off and an on of one tick either way round)
                if !self.offs.contains(&key) {
                    self.offs.push(key);
                }
            }
            Midi::Cc { cc, value, .. } => self.ccs.push((cc, value)),
            Midi::Program { program, .. } => self.program = Some(program),
            Midi::Bend { value, .. } => self.bend = Some(value),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.ccs.is_empty() && self.program.is_none() && self.bend.is_none() && self.offs.is_empty() && self.ons.is_empty()
    }
}

/// A channel's controller values (they persist from tick to tick).
#[derive(Debug, Clone)]
struct Chan {
    cc: [u8; 128],
    /// Driver values seen (CC 20-22): the General MIDI ones are ignored.
    driver: bool,
}

impl Default for Chan {
    fn default() -> Self {
        let mut cc = [0u8; 128];
        cc[7] = 127;
        cc[11] = 127;
        cc[10] = 64;
        cc[101] = 127;
        cc[100] = 127;
        Chan { cc, driver: false }
    }
}

/// What a batch turns into.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Commands {
    /// A new header (song volume, reverb, flags) before the tick.
    pub header: Option<ExtHeader>,
    /// Driver commands for the channel's track, in order.
    pub cmds: Vec<Vec<u8>>,
    /// CC 120: every voice off now.
    pub sound_off: bool,
}

fn varlen(v: u32) -> Vec<u8> {
    let mut x = v;
    let mut out = vec![(x & 0x7f) as u8];
    x >>= 7;
    while x != 0 {
        out.insert(0, (x & 0x7f) as u8 | 0x80);
        x >>= 7;
    }
    out
}

/// The translator's state: the channels' controllers.
#[derive(Debug, Clone)]
pub struct Translator {
    chans: Vec<Chan>,
    /// Sounding keys per channel (CC 123).
    keys: Vec<u128>,
}

impl Default for Translator {
    fn default() -> Self {
        Translator { chans: vec![Chan::default(); TRACKS], keys: vec![0; TRACKS] }
    }
}

impl Translator {
    /// The commands of one channel's batch; `header` is the song's header
    /// now (the header controllers change it).
    pub fn commands(&mut self, ch: usize, b: &Batch, header: ExtHeader) -> Commands {
        let mut out = Commands::default();
        let c = &mut self.chans[ch % TRACKS];
        let mut touched = [false; 128];
        let mut restrike: Vec<u8> = Vec::new();
        let mut vel0: Vec<u8> = Vec::new();
        let mut double: Vec<u8> = Vec::new();
        // RPN 0 (bend range) as its data entry arrives: the export ends
        // each RPN with the null RPN in the same tick
        let mut bend_range: Option<u8> = None;
        for &(n, v) in &b.ccs {
            let n = usize::from(n & 0x7f);
            c.cc[n] = v & 0x7f;
            touched[n] = true;
            if (n == 6 || n == 38) && c.cc[101] == 0 && c.cc[100] == 0 {
                bend_range = Some(c.cc[6] | c.cc[38] << 7);
            }
            if n == usize::from(cc::RESTRIKE) {
                restrike.push(v);
            }
            if n == usize::from(cc::VEL0) {
                vel0.push(v);
            }
            if n == usize::from(cc::DOUBLE) {
                double.push(v);
            }
            if (20..=22).contains(&n) || (52..=54).contains(&n) {
                c.driver = true;
            }
        }
        let v14 = |c: &Chan, msb: u8| u32::from(c.cc[usize::from(msb)]) << 7 | u32::from(c.cc[usize::from(msb) + 32]);
        let t = |n: u8| touched[usize::from(n)];
        let t14 = |n: u8| touched[usize::from(n)] || touched[usize::from(n) + 32];
        let cmds = &mut out.cmds;

        // header and the song's values
        let mut h = header;
        if t(cc::HDR_VOLUME) {
            h.volume = c.cc[usize::from(cc::HDR_VOLUME)];
        }
        if t(cc::HDR_REVERB) {
            let m = c.cc[usize::from(cc::HDR_REVERB)];
            h.reverb_mode = if m < 10 { m } else { 0xff };
        }
        if t14(cc::HDR_DEPTH) {
            h.reverb_depth = (v14(c, cc::HDR_DEPTH) * 2).min(0x7fff) as i16;
        }
        if t(cc::HDR_FLAGS) {
            h.flags = if c.cc[usize::from(cc::HDR_FLAGS)] >= 64 { h.flags | 0x10000 } else { h.flags & !0x10000 };
        }
        if h != header {
            out.header = Some(h);
        }
        if t14(cc::TEMPO) {
            cmds.push(vec![0x08, v14(c, cc::TEMPO).clamp(1, 255) as u8]);
        }
        if t14(cc::DEPTH) {
            cmds.push(vec![0x0a, v14(c, cc::DEPTH) as u8]);
        }
        if let Some(p) = b.program {
            cmds.push(vec![0x20, p]);
        }
        // the envelope
        if t(cc::ADSR_RESET) {
            cmds.push(vec![0x30]);
        }
        for i in 0..8u8 {
            if t(cc::ADSR + i) {
                cmds.push(vec![0x31 + i, c.cc[usize::from(cc::ADSR + i)]]);
            }
        }
        // volume, expression, pan, mix
        let le = |op: u8, v: i32| {
            let b = (v as i16).to_le_bytes();
            vec![op, b[0], b[1]]
        };
        if c.driver {
            if t14(cc::VOLUME) {
                cmds.push(le(EXT_VOLUME, v14(c, cc::VOLUME) as i32));
            }
            if t14(cc::EXPRESSION) {
                cmds.push(le(EXT_EXPRESSION, v14(c, cc::EXPRESSION) as i32));
            }
            if t14(cc::PAN) {
                cmds.push(le(EXT_PAN, v14(c, cc::PAN) as i32 - 0x1f80));
            }
        } else {
            // General MIDI files: CC7, CC11, CC10, CC91
            let lin = |x: u8| (i32::from(x) * 0x3fff + 63) / 127;
            if t(7) {
                cmds.push(le(EXT_VOLUME, lin(c.cc[7])));
            }
            if t(11) {
                cmds.push(le(EXT_EXPRESSION, lin(c.cc[11])));
            }
            if t(10) {
                cmds.push(le(EXT_PAN, i32::from(c.cc[10]) + 0x40));
            }
            if t(91) && !t(cc::MIX) {
                cmds.push(vec![EXT_MIX, if c.cc[91] >= 64 { 0xf } else { 0x3 }]);
            }
        }
        if t(cc::MIX) {
            cmds.push(vec![EXT_MIX, c.cc[usize::from(cc::MIX)] & 0xf]);
        }
        if t(64) {
            cmds.push(vec![0x3c, c.cc[64]]);
        }
        // pitch: bend range (RPN 0), bend, detune, scale
        if let Some(r) = bend_range {
            cmds.push(vec![0x5d, r]);
        }
        if let Some(v) = b.bend {
            cmds.push(vec![0x5c, (v & 0x7f) as u8, (v >> 7) as u8]);
        }
        if t14(cc::DETUNE) {
            cmds.push(vec![0x5a, v14(c, cc::DETUNE) as u8]);
        }
        if t14(cc::SCALE) {
            cmds.push(vec![0x58, v14(c, cc::SCALE) as u8]);
        }
        // voice flags
        for (n, op) in [(cc::VOICE_FLAG_20, 0x3e), (cc::VOICE_FLAG_10, 0x3f), (cc::VOICE_START, 0x3d)] {
            if t(n) {
                cmds.push(vec![op, c.cc[usize::from(n)]]);
            }
        }
        // portamento and pitch slide
        if t14(cc::PORTA) {
            let n = v14(c, cc::PORTA);
            cmds.push(if n == 0x3fff { vec![0x29] } else { [vec![0x28], varlen(n)].concat() });
        }
        if t14(cc::SLIDE_TO) {
            cmds.push([vec![0x2c], varlen(v14(c, cc::SLIDE_TIME)), vec![v14(c, cc::SLIDE_TO) as u8]].concat());
        }
        // LFOs
        let b8 = |c: &Chan, n: u8| c.cc[usize::from(n)];
        if t(cc::PLFO_FADE) {
            cmds.push(vec![0x47, b8(c, cc::PLFO_DELAY), b8(c, cc::PLFO_FADE)]);
        }
        if t(cc::PLFO_WAVE) {
            let w = b8(c, cc::PLFO_WAVE);
            cmds.push(if w == cc::LFO_OFF { vec![0x41] } else { vec![0x40, b8(c, cc::PLFO_DEPTH), b8(c, cc::PLFO_RATE), w] });
        }
        if t(cc::PLFO_FADE_AFTER) {
            cmds.push(vec![0x47, b8(c, cc::PLFO_DELAY_AFTER), b8(c, cc::PLFO_FADE_AFTER)]);
        }
        if t(cc::PANLFO_WAVE) {
            let w = b8(c, cc::PANLFO_WAVE);
            cmds.push(if w == cc::LFO_OFF { vec![0x51] } else { vec![0x50, b8(c, cc::PANLFO_DEPTH), b8(c, cc::PANLFO_RATE), w] });
        }
        if t(cc::TIE) {
            cmds.push(vec![if c.cc[usize::from(cc::TIE)] >= 64 { 0x2a } else { 0x2b }]);
        }
        // notes
        let keys = &mut self.keys[ch % TRACKS];
        if t(123) {
            for k in 0..128u8 {
                if *keys >> k & 1 != 0 {
                    cmds.push(vec![0x1a, k]);
                }
            }
            *keys = 0;
        }
        out.sound_off = t(120);
        for &k in &b.offs {
            if restrike.contains(&k) {
                continue;
            }
            cmds.push(vec![0x1a, k]);
            *keys &= !(1u128 << k);
        }
        for &(k, v) in &b.ons {
            cmds.push(vec![0x11, k, if vel0.contains(&k) { 0 } else { v }]);
            *keys |= 1u128 << k;
        }
        // keys struck twice in the tick: again, after the others (a chord
        // played twice)
        for &(k, v) in &b.ons {
            if double.contains(&k) {
                cmds.push(vec![0x11, k, if vel0.contains(&k) { 0 } else { v }]);
            }
        }
        if t(cc::TRACK_END) {
            cmds.push(vec![0x00]);
        }
        if t(cc::SONG_END) {
            cmds.push(vec![EXT_SONG_END]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_within_a_tick_does_not_matter() {
        let msgs = [
            Midi::NoteOff { ch: 0, key: 60 },
            Midi::NoteOn { ch: 0, key: 62, vel: 90 },
            Midi::Cc { ch: 0, cc: cc::TIE, value: 127 },
            Midi::Program { ch: 0, program: 3 },
            Midi::Cc { ch: 0, cc: cc::VOLUME, value: 0x40 },
            Midi::Cc { ch: 0, cc: cc::VOLUME + 32, value: 0x11 },
            Midi::Cc { ch: 0, cc: cc::PLFO_WAVE, value: 1 },
            Midi::Cc { ch: 0, cc: cc::PLFO_DEPTH, value: 30 },
            Midi::Cc { ch: 0, cc: cc::PLFO_RATE, value: 10 },
        ];
        let run = |order: &[usize]| {
            let mut b = Batch::default();
            for &i in order {
                b.push(msgs[i]);
            }
            Translator::default().commands(0, &b, ExtHeader::default())
        };
        let a = run(&[0, 1, 2, 3, 4, 5, 6, 7, 8]);
        let b = run(&[8, 7, 6, 5, 4, 3, 2, 1, 0]);
        assert_eq!(a, b);
        assert_eq!(
            a.cmds,
            vec![
                vec![0x20, 3],
                vec![EXT_VOLUME, 0x11, 0x20],
                vec![0x40, 30, 10, 1],
                vec![0x2a],
                vec![0x1a, 60],
                vec![0x11, 62, 90],
            ]
        );
    }

    #[test]
    fn restrike_keeps_the_old_voice() {
        let mut b = Batch::default();
        b.push(Midi::NoteOff { ch: 2, key: 60 });
        b.push(Midi::NoteOn { ch: 2, key: 60, vel: 100 });
        b.push(Midi::Cc { ch: 2, cc: cc::RESTRIKE, value: 60 });
        let c = Translator::default().commands(2, &b, ExtHeader::default());
        assert_eq!(c.cmds, vec![vec![0x11, 60, 100]]);
        // without the marker: key-off, then key-on
        let mut b = Batch::default();
        b.push(Midi::NoteOff { ch: 2, key: 60 });
        b.push(Midi::NoteOn { ch: 2, key: 60, vel: 100 });
        let c = Translator::default().commands(2, &b, ExtHeader::default());
        assert_eq!(c.cmds, vec![vec![0x1a, 60], vec![0x11, 60, 100]]);
    }

    #[test]
    fn general_midi_controllers_without_driver_values() {
        let mut b = Batch::default();
        b.push(Midi::Cc { ch: 0, cc: 7, value: 127 });
        b.push(Midi::Cc { ch: 0, cc: 10, value: 0 });
        let c = Translator::default().commands(0, &b, ExtHeader::default());
        assert_eq!(c.cmds, vec![vec![EXT_VOLUME, 0xff, 0x3f], vec![EXT_PAN, 0x40, 0]]);
    }
}
