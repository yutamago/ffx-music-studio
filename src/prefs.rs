//! The window's own settings, kept across tracks and restarts in
//! `ffx-music-studio.toml` next to the exe: the output volume, the
//! metronome, the MIDI input, the zoom and the snap. Not track edits (those
//! are never saved).

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct Prefs {
    /// Output volume (after the synth and the limiter), 0..2.
    pub output: f32,
    pub metronome: bool,
    /// The MIDI input's name ("" : the first one; "-": none).
    pub midi: String,
    /// Piano roll: pixels per tick, key height; channel rack: row height.
    pub roll_px_per_tick: f32,
    pub roll_row_h: f32,
    pub rack_row_h: f32,
    /// Snap, in ticks.
    pub grid: u32,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs { output: 1.0, metronome: false, midi: String::new(), roll_px_per_tick: 0.0, roll_row_h: 8.0, rack_row_h: 34.0, grid: 12 }
    }
}

impl Prefs {
    pub fn path() -> PathBuf {
        demaster::build::exe_dir().join("ffx-music-studio.toml")
    }

    pub fn parse(text: &str) -> Prefs {
        let mut p = Prefs::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            let s = v.trim_matches('"').replace("\\\"", "\"").replace("\\\\", "\\");
            let f = v.parse::<f32>().ok().filter(|x| x.is_finite());
            match k {
                "output" => p.output = f.map_or(p.output, |x| x.clamp(0.0, 2.0)),
                "metronome" => p.metronome = v == "true",
                "midi" => p.midi = s,
                "roll_px_per_tick" => p.roll_px_per_tick = f.map_or(0.0, |x| x.clamp(0.0, 16.0)),
                "roll_row_h" => p.roll_row_h = f.map_or(p.roll_row_h, |x| x.clamp(3.0, 40.0)),
                "rack_row_h" => p.rack_row_h = f.map_or(p.rack_row_h, |x| x.clamp(16.0, 120.0)),
                "grid" => p.grid = v.parse().map_or(p.grid, |g: u32| g.clamp(1, 192)),
                _ => {}
            }
        }
        p
    }

    pub fn to_text(&self) -> String {
        let q = |s: &str| format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""));
        format!(
            "# FFX Music Studio's window settings (written by the studio)\noutput = {}\nmetronome = {}\nmidi = {}\nroll_px_per_tick = {}\nroll_row_h = {}\nrack_row_h = {}\ngrid = {}\n",
            self.output,
            self.metronome,
            q(&self.midi),
            self.roll_px_per_tick,
            self.roll_row_h,
            self.rack_row_h,
            self.grid
        )
    }

    pub fn load(path: &Path) -> Prefs {
        std::fs::read_to_string(path).map(|t| Prefs::parse(&t)).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        std::fs::write(path, self.to_text())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_bad_values() {
        let p = Prefs { output: 0.35, metronome: true, midi: "USB \"Keys\" \\ 1".into(), roll_px_per_tick: 0.75, roll_row_h: 11.0, rack_row_h: 50.0, grid: 24 };
        assert_eq!(Prefs::parse(&p.to_text()), p);
        let q = Prefs::parse("output = 9\nroll_row_h = x\nunknown = 1\ngrid = 0");
        assert_eq!(q.output, 2.0);
        assert_eq!(q.roll_row_h, Prefs::default().roll_row_h);
        assert_eq!(q.grid, 1);
        assert_eq!(Prefs::parse(""), Prefs::default());
    }

    #[test]
    fn saved_next_to_the_exe_and_read_back() {
        let dir = std::env::temp_dir().join(format!("ffx-studio-prefs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("ffx-music-studio.toml");
        let p = Prefs { output: 0.5, ..Prefs::default() };
        p.save(&f).unwrap();
        assert_eq!(Prefs::load(&f).output, 0.5);
        assert_eq!(Prefs::load(&dir.join("missing.toml")), Prefs::default());
        std::fs::remove_dir_all(&dir).ok();
    }
}
