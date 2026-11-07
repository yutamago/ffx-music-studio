//! Where the instruments come from: the player's own disc (through a
//! `demaster.toml` / `ffx-music-studio.toml`, as FFX Music Studio finds it)
//! or the folder `ffx-music-studio --export` wrote (its `banks/`). Nothing
//! of the game ships with the plugin.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use demaster::config::Config;
use demaster::disc::Disc;
use demaster::musicjob::{Sources, TRACKS};
use demaster::sqseq::{Bgm, ExtHeader, Tables, Wd};

/// One sequence of the disc.
#[derive(Debug, Clone)]
pub struct Track {
    pub id: u16,
    pub title: String,
    pub wd: u16,
    pub header: ExtHeader,
}

impl Track {
    pub fn label(&self) -> String {
        if self.title.is_empty() { format!("music{:03}", self.id) } else { format!("music{:03} {}", self.id, self.title) }
    }
}

enum Backend {
    Disc { disc: Box<Disc>, src: Box<Sources> },
    Folder(PathBuf),
}

pub struct Library {
    backend: Backend,
    pub tables: Arc<Tables>,
    pub tracks: Vec<Track>,
    /// The banks the music uses, by the export's bank number (0, 1, ...).
    pub banks: Vec<u16>,
    /// Where it came from (shown in the status).
    pub origin: String,
    cache: BTreeMap<u16, Arc<Wd>>,
}

/// "1_09_battle_theme" -> "Battle Theme (1-09)" (FFX Music Studio's names).
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
            if i > 0 && ["of", "the", "in", "at", "to", "a", "de"].contains(&w) {
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

/// The folder the plugin's own file is in (the DLL, not the host's exe).
pub fn module_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStringExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetModuleHandleExW(flags: u32, name: *const u16, module: *mut *mut core::ffi::c_void) -> i32;
            fn GetModuleFileNameW(module: *mut core::ffi::c_void, buf: *mut u16, size: u32) -> u32;
        }
        const FROM_ADDRESS: u32 = 4;
        const UNCHANGED_REFCOUNT: u32 = 2;
        let mut m = std::ptr::null_mut();
        let addr = module_dir as *const () as *const u16;
        // SAFETY: plain Win32 calls with a valid out pointer and buffer.
        unsafe {
            if GetModuleHandleExW(FROM_ADDRESS | UNCHANGED_REFCOUNT, addr, &mut m) == 0 {
                return None;
            }
            let mut buf = vec![0u16; 4096];
            let n = GetModuleFileNameW(m, buf.as_mut_ptr(), buf.len() as u32) as usize;
            if n == 0 {
                return None;
            }
            let p = PathBuf::from(std::ffi::OsString::from_wide(&buf[..n]));
            p.parent().map(Path::to_path_buf)
        }
    }
    #[cfg(not(windows))]
    {
        std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf))
    }
}

/// The places looked at, in order, when no source is set: `FFX_PS2_SOUND`,
/// then next to the plugin file and up to four folders above it (the VST3
/// bundle's own folders, then the folder holding the bundle), then the
/// user's settings folder; in each the files `ffx-ps2-sound.toml`,
/// `ffx-music-studio.toml`, `demaster.toml`, or an export folder.
pub fn candidates() -> Vec<PathBuf> {
    let mut c = Vec::new();
    if let Some(p) = std::env::var_os("FFX_PS2_SOUND") {
        c.push(PathBuf::from(p));
    }
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = module_dir() {
        dirs.extend(d.ancestors().take(5).map(Path::to_path_buf));
    }
    if let Some(a) = std::env::var_os("APPDATA") {
        dirs.push(PathBuf::from(a).join("FFX PS2 Sound"));
    }
    if let Some(h) = std::env::var_os("HOME") {
        dirs.push(PathBuf::from(h).join(".config").join("ffx-ps2-sound"));
    }
    for d in dirs {
        // an export folder holding the plugin (`<export>\plugin\`)
        c.push(d.clone());
        for f in ["ffx-ps2-sound.toml", "ffx-music-studio.toml", "demaster.toml"] {
            c.push(d.join(f));
        }
    }
    c
}

fn is_export(p: &Path) -> bool {
    p.join("banks").join("iopsound.irx").is_file()
}

impl Library {
    /// Opens `path` (a config file, an export folder) or, when empty, the
    /// first of [`candidates`] that exists.
    pub fn open(path: &str) -> Result<Library> {
        let path = path.trim().trim_matches('"');
        if !path.is_empty() {
            return Self::open_at(Path::new(path));
        }
        let cands = candidates();
        // a candidate that fails (a studio settings file without [source],
        // a disc image that moved) does not stop the search
        let mut errors = Vec::new();
        for c in &cands {
            if c.is_file() || (c.is_dir() && is_export(c)) {
                match Self::open_at(c) {
                    Ok(l) => return Ok(l),
                    Err(e) => errors.push(format!("  {}: {e:#}", c.display())),
                }
            }
        }
        bail!(
            "no disc or export set. Set the source to a demaster.toml / ffx-music-studio.toml (its [source] iso is your PS2 disc \
             image) or to an `ffx-music-studio --export` folder, or put ffx-ps2-sound.toml in one of:\n{}{}",
            cands.iter().map(|p| format!("  {}", p.display())).collect::<Vec<_>>().join("\n"),
            if errors.is_empty() { String::new() } else { format!("\ntried and failed:\n{}", errors.join("\n")) }
        )
    }

    fn open_at(p: &Path) -> Result<Library> {
        if p.is_dir() {
            if is_export(p) {
                return Self::open_folder(p);
            }
            bail!("{}: not an export folder with banks/ (run ffx-music-studio --export again)", p.display());
        }
        Self::open_config(p)
    }

    fn open_config(path: &Path) -> Result<Library> {
        let loaded = Config::load(path)?;
        let iso = loaded
            .resolve(&loaded.config.source.iso)
            .with_context(|| format!("{}: `iso` under [source] is empty: set it to your PS2 disc image", path.display()))?;
        if !iso.is_file() {
            bail!("{}: the PS2 disc image {} does not exist", path.display(), iso.display());
        }
        let vbf = demaster::build::find_vbf(None, &loaded)?;
        let pc = ffx_vbf::Archive::open(&vbf).with_context(|| format!("opening {}", vbf.display()))?;
        let mut disc = Disc::open(&iso, &pc)?;
        let src = Sources::load(&mut disc)?;
        let tables = Arc::new(src.tables.clone());
        let have = src.bank_ids();
        let mut tracks = Vec::new();
        for id in src.music_ids(&disc) {
            let b = src.sequence(&mut disc, id)?;
            let bgm = Bgm::parse(&b).with_context(|| format!("music{id:03}"))?;
            let pc = TRACKS.iter().find(|t| t.1 == id).map_or("", |t| t.0);
            tracks.push(Track { id, title: if pc.is_empty() { String::new() } else { title(pc) }, wd: bgm.wd, header: ExtHeader::of(&bgm) });
        }
        let mut banks: Vec<u16> = tracks.iter().map(|t| t.wd).filter(|w| have.contains(w)).collect();
        banks.sort_unstable();
        banks.dedup();
        Ok(Library {
            backend: Backend::Disc { disc: Box::new(disc), src: Box::new(src) },
            tables,
            tracks,
            banks,
            origin: format!("disc {}", iso.display()),
            cache: BTreeMap::new(),
        })
    }

    fn open_folder(dir: &Path) -> Result<Library> {
        let b = dir.join("banks");
        let rd = |n: &str| std::fs::read(b.join(n)).with_context(|| b.join(n).display().to_string());
        let tables = Arc::new(Tables::from_irx(&rd("iopsound.irx")?, &rd("libsd.irx")?)?);
        let j: serde_json::Value = serde_json::from_slice(&std::fs::read(dir.join("tracks.json")).context("tracks.json")?)?;
        let mut tracks = Vec::new();
        for r in j.as_array().context("tracks.json")? {
            let h = &r["header"];
            let n = |v: &serde_json::Value, d: i64| v.as_i64().unwrap_or(d);
            tracks.push(Track {
                id: n(&r["id"], 0) as u16,
                title: r["title"].as_str().unwrap_or("").replace(" (proposed)", ""),
                wd: n(&r["wd"], 0) as u16,
                header: ExtHeader {
                    volume: n(&h["volume"], 0x7f) as u8,
                    reverb_mode: n(&h["reverb_mode"], 0) as u8,
                    reverb_depth: n(&h["reverb_depth"], 0) as i16,
                    flags: n(&h["flags"], 0) as u32,
                },
            });
        }
        let mut banks: Vec<u16> = std::fs::read_dir(&b)?
            .filter_map(|e| e.ok()?.file_name().to_str()?.strip_prefix("wd")?.strip_suffix(".wd")?.parse().ok())
            .collect();
        banks.sort_unstable();
        Ok(Library { backend: Backend::Folder(b), tables, tracks, banks, origin: format!("export {}", dir.display()), cache: BTreeMap::new() })
    }

    pub fn bank(&mut self, id: u16) -> Result<Arc<Wd>> {
        if let Some(w) = self.cache.get(&id) {
            return Ok(w.clone());
        }
        let w = match &mut self.backend {
            Backend::Disc { disc, src } => src.bank(disc, id)?,
            Backend::Folder(b) => {
                let p = b.join(format!("wd{id:03}.wd"));
                Wd::parse(&std::fs::read(&p).with_context(|| p.display().to_string())?)?
            }
        };
        let w = Arc::new(w);
        self.cache.insert(id, w.clone());
        Ok(w)
    }

    /// The bytes of `musicNNN.bgm` (from a disc only).
    pub fn sequence(&mut self, id: u16) -> Option<Vec<u8>> {
        match &mut self.backend {
            Backend::Disc { disc, src } => src.sequence(disc, id).ok(),
            Backend::Folder(_) => None,
        }
    }

    pub fn track(&self, id: u16) -> Option<&Track> {
        self.tracks.iter().find(|t| t.id == id)
    }
}
