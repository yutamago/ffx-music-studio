//! The player's disc: found through `demaster.toml`, its music tracks with
//! names, its instrument banks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use demaster::config::Config;
use demaster::disc::Disc;
use demaster::musicjob::{Sources, TRACKS};
use demaster::sqseq::{Bgm, Tables, Wd};

/// Where `demaster.toml` was looked for, in order: `--config`, next to the
/// exe, then the mod folder layout (the exe in `mods/yuta.demaster/` or in a
/// folder next to it, `tools/` for example), then the current folder.
pub fn config_candidates(arg: Option<&Path>) -> Vec<PathBuf> {
    let mut c = Vec::new();
    if let Some(a) = arg {
        c.push(a.to_path_buf());
        return c;
    }
    let exe = demaster::build::exe_dir();
    c.push(exe.join("demaster.toml"));
    for up in exe.ancestors().skip(1).take(3) {
        c.push(up.join("demaster.toml"));
        c.push(up.join("mods").join("yuta.demaster").join("demaster.toml"));
        c.push(up.join("yuta.demaster").join("demaster.toml"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        c.push(cwd.join("demaster.toml"));
    }
    c.dedup();
    c
}

/// One track of the list.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    pub id: u16,
    /// "Battle Theme" (from the PC soundtrack's sample name), or "".
    pub title: String,
    /// The PC bank's sample name ("1_09_battle_theme"), or "".
    pub pc_name: String,
}

impl TrackInfo {
    pub fn label(&self) -> String {
        if self.title.is_empty() {
            format!("music{:03}", self.id)
        } else {
            format!("{} \u{2014} music{:03}", self.title, self.id)
        }
    }

    /// Matches a `--render` argument: the id, `musicNNN`, or part of the
    /// name.
    pub fn matches(&self, q: &str) -> bool {
        let q = q.to_ascii_lowercase();
        if let Ok(n) = q.trim_start_matches("music").parse::<u16>() {
            return n == self.id;
        }
        self.pc_name.contains(&q) || self.title.to_ascii_lowercase().contains(&q.replace('_', " "))
    }
}

/// "1_04_tidus_s_theme" -> "Tidus's Theme (1-04)".
pub fn title(pc_name: &str) -> String {
    let mut parts = pc_name.splitn(3, '_');
    let (disc, num, rest) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (rest, oneshot) = match rest.strip_suffix("_oneshot") {
        Some(r) => (r, true),
        None => (rest, false),
    };
    let words: Vec<String> = rest
        .replace("_s_", "'s_")
        .split('_')
        .filter(|w| !w.is_empty())
        .enumerate()
        .map(|(i, w)| {
            let small = ["of", "the", "in", "at", "to", "a", "de"].contains(&w);
            if small && i > 0 {
                w.to_string()
            } else {
                let mut c = w.chars();
                c.next().map_or(String::new(), |f| f.to_uppercase().collect::<String>() + c.as_str())
            }
        })
        .collect();
    let mut t = words.join(" ");
    if oneshot {
        t.push_str(" (one-shot)");
    }
    if !disc.is_empty() {
        t.push_str(&format!(" ({disc}-{num})"));
    }
    t
}

pub struct Library {
    pub disc: Disc,
    pub src: Sources,
    pub tables: Arc<Tables>,
    pub tracks: Vec<TrackInfo>,
    pub banks: Vec<u16>,
    pub config_path: PathBuf,
    pub iso: PathBuf,
    bank_cache: BTreeMap<u16, Arc<Wd>>,
}

impl Library {
    /// Opens the disc named by the first `demaster.toml` found.
    pub fn open(config_arg: Option<&Path>) -> Result<Library> {
        let cands = config_candidates(config_arg);
        let Some(path) = cands.iter().find(|p| p.is_file()) else {
            bail!(
                "no demaster.toml found. Looked at:\n{}\n\nPut a demaster.toml (as written by demaster.exe) next to ffx-music-studio.exe, \
                 with `iso` under [source] set to your PS2 disc image, or start it with --config <file>.",
                cands.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n")
            );
        };
        let loaded = Config::load(path)?;
        let iso = loaded.resolve(&loaded.config.source.iso).with_context(|| {
            format!("{}: `iso` under [source] is empty. Set it to your PS2 disc image of Final Fantasy X.", path.display())
        })?;
        if !iso.is_file() {
            bail!("{}: the PS2 disc image {} does not exist", path.display(), iso.display());
        }
        let vbf = demaster::build::find_vbf(None, &loaded)?;
        let pc = ffx_vbf::Archive::open(&vbf).with_context(|| format!("opening {}", vbf.display()))?;
        let mut disc = Disc::open(&iso, &pc)?;
        let src = Sources::load(&mut disc)?;
        let tables = Arc::new(src.tables.clone());
        let mut tracks: Vec<TrackInfo> = src
            .music_ids(&disc)
            .into_iter()
            .map(|id| {
                let pc_name = TRACKS.iter().find(|t| t.1 == id).map_or("", |t| t.0).to_string();
                TrackInfo { id, title: if pc_name.is_empty() { String::new() } else { title(&pc_name) }, pc_name }
            })
            .collect();
        sort_tracks(&mut tracks);
        let banks = src.bank_ids();
        Ok(Library { disc, src, tables, tracks, banks, config_path: path.clone(), iso, bank_cache: BTreeMap::new() })
    }

    /// The sequence's bytes and its instrument bank. A bank that is not on
    /// the disc (10 sequences name one) comes back empty, with a note.
    pub fn song(&mut self, id: u16) -> Result<(Vec<u8>, Arc<Wd>, Option<String>)> {
        let name = format!("music{id:03}");
        let b = self.src.sequence(&mut self.disc, id).with_context(|| name.clone())?;
        let bgm = Bgm::parse(&b).with_context(|| name.clone())?;
        match self.bank(bgm.wd) {
            Ok(wd) => Ok((b, wd, None)),
            Err(e) => {
                let empty = Wd { id: bgm.wd, instruments: Vec::new(), regions: Vec::new(), samples: Vec::new() };
                Ok((b, Arc::new(empty), Some(format!("{name}: {e:#}: silent until its channels get instruments from other banks"))))
            }
        }
    }

    pub fn bank(&mut self, id: u16) -> Result<Arc<Wd>> {
        if let Some(w) = self.bank_cache.get(&id) {
            return Ok(w.clone());
        }
        let w = Arc::new(self.src.bank(&mut self.disc, id)?);
        self.bank_cache.insert(id, w.clone());
        Ok(w)
    }

    pub fn find(&self, q: &str) -> Option<&TrackInfo> {
        self.tracks.iter().find(|t| t.matches(q))
    }
}

/// Named tracks first, in the soundtrack's order; then the others by id.
pub fn sort_tracks(v: &mut [TrackInfo]) {
    v.sort_by_key(|t| (t.pc_name.is_empty(), t.pc_name.clone(), t.id));
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn titles() {
        assert_eq!(title("1_09_battle_theme"), "Battle Theme (1-09)");
        assert_eq!(title("1_04_tidus_s_theme"), "Tidus's Theme (1-04)");
        assert_eq!(title("1_26_good_night_oneshot"), "Good Night (one-shot) (1-26)");
        assert_eq!(title("2_12_auron_s_theme"), "Auron's Theme (2-12)");
        assert_eq!(title("m_05_the_temple_band"), "The Temple Band (m-05)");
        assert_eq!(title("3_04_sea_of_mists"), "Sea of Mists (3-04)");
    }

    #[test]
    fn named_tracks_first() {
        let t = |id, n: &str| TrackInfo { id, title: String::new(), pc_name: n.into() };
        let mut v = [t(5, ""), t(16, "1_09_battle_theme"), t(1, ""), t(130, "1_02_zanarkand")];
        sort_tracks(&mut v);
        let ids: Vec<u16> = v.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![130, 16, 1, 5]);
    }
}
