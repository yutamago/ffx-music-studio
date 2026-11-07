//! The nih-plug wrapper (VST3 and CLAP): parameters, the bank loading in
//! the background, linked instances (parts), and `process()` handing the
//! host's block to [`Engine::process`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock, Weak};

use nih_plug::prelude::*;

use crate::engine::{Engine, Event, Instrument, Settings, TempoSource, Transport as EngTransport};
use crate::midi::Midi;
use crate::source::Library;

#[derive(Enum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TempoParam {
    #[name = "Sequence (PS2 timing)"]
    Sequence,
    #[name = "Host"]
    Host,
}

#[derive(Params)]
pub struct SoundParams {
    /// Where the instruments come from: a demaster.toml /
    /// ffx-music-studio.toml / ffx-ps2-sound.toml, or an export folder.
    /// Empty: looked for (README of the plugin, `source::candidates`).
    #[persist = "source"]
    pub source: Mutex<String>,
    /// The track whose bank (and header: song volume, reverb) is played;
    /// 0: the bank by number.
    #[id = "track"]
    pub track: IntParam,
    /// The bank by the export's number (the MIDI files' bank select).
    #[id = "bank"]
    pub bank: IntParam,
    /// Which 16 driver tracks this instance's MIDI channels are (1: tracks
    /// 0-15, 2: 16-31, 3: 32-47).
    #[id = "part"]
    pub part: IntParam,
    /// 0: on its own; 1-16: instances with the same link play on one chip
    /// (part 1 sounds, the others send it their MIDI).
    #[id = "link"]
    pub link: IntParam,
    #[id = "tempo"]
    pub tempo: EnumParam<TempoParam>,
    /// The studio's output stage: +9 dB and the limiter at -1 dBFS.
    #[id = "stage"]
    pub stage: BoolParam,
    #[id = "gain"]
    pub gain: FloatParam,
}

/// Names of the tracks, for the track parameter's display.
type Names = Arc<RwLock<BTreeMap<i32, String>>>;

impl SoundParams {
    fn new(names: Names) -> Self {
        let n = names.clone();
        SoundParams {
            source: Mutex::new(String::new()),
            track: IntParam::new("Track", 16, IntRange::Linear { min: 0, max: 255 }).with_value_to_string(Arc::new(move |v| {
                if v == 0 {
                    return "by bank".into();
                }
                n.read().ok().and_then(|m| m.get(&v).cloned()).unwrap_or_else(|| format!("music{v:03}"))
            }))
            .with_string_to_value(Arc::new(|s| {
                let s = s.trim();
                if s == "by bank" {
                    return Some(0);
                }
                let d: String = s.trim_start_matches("music").chars().take_while(char::is_ascii_digit).collect();
                d.parse().ok()
            })),
            bank: IntParam::new("Bank", 0, IntRange::Linear { min: 0, max: 127 }),
            part: IntParam::new("Part", 1, IntRange::Linear { min: 1, max: 3 }),
            link: IntParam::new("Link", 0, IntRange::Linear { min: 0, max: 16 })
                .with_value_to_string(Arc::new(|v| if v == 0 { "off".into() } else { format!("group {v}") }))
                .with_string_to_value(Arc::new(|s| {
                    let s = s.trim();
                    if s == "off" { Some(0) } else { s.trim_start_matches("group").trim().parse().ok() }
                })),
            tempo: EnumParam::new("Tempo", TempoParam::Sequence),
            stage: BoolParam::new("PS2 output stage", true),
            gain: FloatParam::new("Gain", 1.0, FloatRange::Skewed { min: 0.0, max: 4.0, factor: 0.5 })
                .with_unit(" x"),
        }
    }
}

/// What to load: (source, track, bank).
type Key = (String, i32, i32);

pub enum Task {
    Load(Key),
}

/// Shared with the background thread.
#[derive(Default)]
struct Loader {
    lib: Mutex<Option<(String, Library)>>,
    ready: Mutex<Option<(Key, Arc<Instrument>)>>,
    status: Mutex<String>,
    /// A load is running (one at a time; the latest settings win).
    busy: std::sync::atomic::AtomicBool,
}

/// Linked instances: events of parts 2 and 3 for part 1's engine.
#[derive(Default)]
struct Group {
    inbox: Mutex<Vec<(Option<i64>, Midi)>>,
}

static GROUPS: Mutex<Vec<(i32, Weak<Group>)>> = Mutex::new(Vec::new());

fn group(id: i32) -> Arc<Group> {
    let mut g = GROUPS.lock().expect("groups");
    g.retain(|(_, w)| w.strong_count() > 0);
    if let Some(a) = g.iter().find(|(i, _)| *i == id).and_then(|(_, w)| w.upgrade()) {
        return a;
    }
    let a = Arc::new(Group::default());
    g.push((id, Arc::downgrade(&a)));
    a
}

pub struct FfxSound {
    params: Arc<SoundParams>,
    names: Names,
    loader: Arc<Loader>,
    engine: Option<Engine>,
    sr: f32,
    max_block: usize,
    want: Option<Key>,
    group: Option<(i32, Arc<Group>)>,
    events: Vec<Event>,
    linked: Vec<(Option<i64>, Midi)>,
}

impl Default for FfxSound {
    fn default() -> Self {
        let names: Names = Arc::default();
        FfxSound {
            params: Arc::new(SoundParams::new(names.clone())),
            names,
            loader: Arc::default(),
            engine: None,
            sr: 48000.0,
            max_block: 512,
            want: None,
            group: None,
            events: Vec::with_capacity(4096),
            linked: Vec::with_capacity(4096),
        }
    }
}

impl FfxSound {
    fn key(&self) -> Key {
        let src = self.params.source.try_lock().map(|s| s.clone()).unwrap_or_default();
        (src, self.params.track.value(), self.params.bank.value())
    }

    fn settings(&self) -> Settings {
        Settings {
            tempo: match self.params.tempo.value() {
                TempoParam::Sequence => TempoSource::Sequence,
                TempoParam::Host => TempoSource::Host,
            },
            stage: self.params.stage.value(),
            gain: self.params.gain.value(),
        }
    }
}

fn load(loader: &Loader, names: &Names, key: &Key) -> anyhow::Result<Arc<Instrument>> {
    let mut slot = loader.lib.lock().expect("lib");
    if slot.as_ref().is_none_or(|(s, _)| *s != key.0) {
        *slot = None;
        let lib = Library::open(&key.0)?;
        if let Ok(mut n) = names.write() {
            n.clear();
            for t in &lib.tracks {
                n.insert(i32::from(t.id), t.label());
            }
        }
        *slot = Some((key.0.clone(), lib));
    }
    let (_, lib) = slot.as_mut().expect("library");
    let (wd, header, label) = if key.1 > 0 {
        let t = lib.track(key.1 as u16).ok_or_else(|| anyhow::anyhow!("no track music{:03} on this disc", key.1))?.clone();
        (t.wd, t.header, t.label())
    } else {
        let w = *lib.banks.get(key.2 as usize).ok_or_else(|| anyhow::anyhow!("no bank {} (the disc's music has {})", key.2, lib.banks.len()))?;
        (w, Default::default(), format!("bank {} (WD {w:03})", key.2))
    };
    let wd = lib.bank(wd)?;
    let inst = Instrument::new(lib.tables.clone(), wd, header, format!("{label}, {}", lib.origin));
    Ok(Arc::new(inst))
}

impl Plugin for FfxSound {
    const NAME: &'static str = "FFX PS2 Sound";
    const VENDOR: &'static str = "FFX Music Studio";
    const URL: &'static str = "https://github.com/yutamago/ffx-music-studio";
    const EMAIL: &'static str = "";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    const AUDIO_IO_LAYOUTS: &'static [AudioIOLayout] =
        &[AudioIOLayout { main_input_channels: None, main_output_channels: NonZeroU32::new(2), ..AudioIOLayout::const_default() }];

    const MIDI_INPUT: MidiConfig = MidiConfig::MidiCCs;
    const SAMPLE_ACCURATE_AUTOMATION: bool = false;

    type SysExMessage = ();
    type BackgroundTask = Task;

    fn params(&self) -> Arc<dyn Params> {
        self.params.clone()
    }

    fn task_executor(&mut self) -> TaskExecutor<Self> {
        let loader = self.loader.clone();
        let names = self.names.clone();
        Box::new(move |task| match task {
            Task::Load(key) => {
                struct Done<'a>(&'a std::sync::atomic::AtomicBool);
                impl Drop for Done<'_> {
                    fn drop(&mut self) {
                        self.0.store(false, std::sync::atomic::Ordering::Release);
                    }
                }
                let _done = Done(&loader.busy);
                let r = load(&loader, &names, &key);
                match r {
                    Ok(inst) => {
                        *loader.status.lock().expect("status") = inst.label.clone();
                        nih_log!("FFX PS2 Sound: {}", inst.label);
                        crate::source::log_line(&format!("loaded {}", inst.label));
                        *loader.ready.lock().expect("ready") = Some((key, inst));
                    }
                    Err(e) => {
                        nih_log!("FFX PS2 Sound: {e:#}");
                        crate::source::log_line(&format!("error: {e:#}"));
                        *loader.status.lock().expect("status") = format!("{e:#}");
                    }
                }
            }
        })
    }

    fn initialize(&mut self, _layout: &AudioIOLayout, cfg: &BufferConfig, context: &mut impl InitContext<Self>) -> bool {
        self.sr = cfg.sample_rate;
        self.max_block = cfg.max_buffer_size as usize;
        let key = self.key();
        self.want = Some(key.clone());
        // a bank loaded before (same settings) stays; else load it
        if self.engine.as_ref().is_none() {
            self.loader.busy.store(true, std::sync::atomic::Ordering::Release);
            context.execute(Task::Load(key));
        }
        let settings = self.settings();
        if let Some(e) = &mut self.engine {
            *e = Engine::new(e.instrument().clone(), f64::from(self.sr), settings);
            e.set_max_block(self.max_block);
            context.set_latency_samples(e.latency());
        } else {
            let lat = ((crate::engine::LOOKAHEAD_S * f64::from(self.sr)).round() as usize).max(2 * self.max_block)
                + crate::engine::STAGE_DELAY * self.sr as usize / 48000;
            context.set_latency_samples(lat as u32);
        }
        true
    }

    fn reset(&mut self) {
        if let Some(e) = &mut self.engine {
            let mut n = Engine::new(e.instrument().clone(), f64::from(self.sr), e.settings);
            n.set_max_block(self.max_block);
            *e = n;
        }
    }

    fn process(&mut self, buffer: &mut Buffer, _aux: &mut AuxiliaryBuffers, context: &mut impl ProcessContext<Self>) -> ProcessStatus {
        // the bank: load when the settings change, take it when ready
        let key = self.key();
        if self.want.as_ref() != Some(&key)
            && self.loader.busy.compare_exchange(false, true, std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire).is_ok()
        {
            self.want = Some(key.clone());
            context.execute_background(Task::Load(key.clone()));
        }
        if let Ok(mut r) = self.loader.ready.try_lock()
            && r.as_ref().is_some_and(|(k, _)| Some(k) == self.want.as_ref())
            && let Some((_, inst)) = r.take()
        {
            let mut e = Engine::new(inst, f64::from(self.sr), self.settings());
            e.set_max_block(self.max_block);
            self.engine = Some(e);
        }
        let settings = self.settings();
        // the latency is the same with the stage on or off (set at activation)
        if let Some(e) = &mut self.engine {
            e.settings = settings;
        }
        // linked instances
        let link = self.params.link.value();
        let part = self.params.part.value().clamp(1, 3) as u8 - 1;
        if link == 0 {
            self.group = None;
        } else if self.group.as_ref().is_none_or(|g| g.0 != link) {
            self.group = Some((link, group(link)));
        }

        let tr = {
            let t = context.transport();
            EngTransport { playing: t.playing, ppq: t.pos_beats(), tempo: t.tempo }
        };
        self.events.clear();
        while let Some(ev) = context.next_event() {
            let at = ev.timing();
            let m = match ev {
                NoteEvent::NoteOn { channel, note, velocity, .. } => {
                    let vel = (velocity * 127.0).round().clamp(0.0, 127.0) as u8;
                    if vel == 0 { Midi::NoteOff { ch: channel, key: note } } else { Midi::NoteOn { ch: channel, key: note, vel } }
                }
                NoteEvent::NoteOff { channel, note, .. } => Midi::NoteOff { ch: channel, key: note },
                NoteEvent::MidiCC { channel, cc, value, .. } => Midi::Cc { ch: channel, cc, value: (value * 127.0).round() as u8 },
                NoteEvent::MidiProgramChange { channel, program, .. } => Midi::Program { ch: channel, program },
                NoteEvent::MidiPitchBend { channel, value, .. } => Midi::Bend { ch: channel, value: (value * 16383.0).round() as u16 },
                _ => continue,
            };
            self.events.push(Event { at, msg: m.on_part(part) });
        }

        let n = buffer.samples();
        let out = buffer.as_slice();
        let renders = self.group.is_none() || part == 0;
        if !renders {
            // part 2/3: the MIDI goes to part 1's chip; silence here
            if let Some((_, g)) = &self.group
                && let Ok(mut inbox) = g.inbox.lock()
            {
                for e in &self.events {
                    let h = match (tr.playing, tr.ppq) {
                        (true, Some(ppq)) => {
                            let tempo = tr.tempo.unwrap_or(120.0);
                            Some(((ppq + f64::from(e.at) / f64::from(self.sr) * tempo / 60.0) * 48.0).round() as i64)
                        }
                        _ => None,
                    };
                    inbox.push((h, e.msg));
                }
            }
            for ch in out.iter_mut() {
                ch.fill(0.0);
            }
            return ProcessStatus::Normal;
        }
        let Some(e) = &mut self.engine else {
            for ch in out.iter_mut() {
                ch.fill(0.0);
            }
            return ProcessStatus::Normal;
        };
        if let Some((_, g)) = &self.group
            && let Ok(mut inbox) = g.inbox.try_lock()
        {
            self.linked.clear();
            self.linked.append(&mut inbox);
        }
        let (l, r) = out.split_at_mut(1);
        let (l, r) = (&mut l[0][..n], &mut r[0][..n]);
        e.process(&tr, &self.events, l, r);
        for (h, m) in self.linked.drain(..) {
            e.push_host(h, m);
        }
        ProcessStatus::Normal
    }
}

impl ClapPlugin for FfxSound {
    const CLAP_ID: &'static str = "io.github.yutamago.ffx-ps2-sound";
    const CLAP_DESCRIPTION: Option<&'static str> = Some("FFX's PS2 sound driver and SPU2, with the instruments of your own disc");
    const CLAP_MANUAL_URL: Option<&'static str> = Some(Self::URL);
    const CLAP_SUPPORT_URL: Option<&'static str> = None;
    const CLAP_FEATURES: &'static [ClapFeature] = &[ClapFeature::Instrument, ClapFeature::Synthesizer, ClapFeature::Stereo];
}

impl Vst3Plugin for FfxSound {
    const VST3_CLASS_ID: [u8; 16] = *b"FfxPs2SoundSpu2!";
    const VST3_SUBCATEGORIES: &'static [Vst3SubCategory] = &[Vst3SubCategory::Instrument, Vst3SubCategory::Synth, Vst3SubCategory::Stereo];
}

nih_export_clap!(FfxSound);
nih_export_vst3!(FfxSound);
