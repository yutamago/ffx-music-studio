//! The window: the track list, the transport, and the tabs (channel rack,
//! piano roll, mixer and instruments, sound chip, song). Edits go into the
//! [`Session`]; [`App::sync`] hands what changed to the audio engine. Notes
//! played on the piano roll's keys or a MIDI keyboard go through the
//! [`Keyboard`] to the engine's live synth.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use anyhow::{Result, anyhow};
use demaster::spu2::Interp;
use demaster::sqseq::{TrackMod, Wd};
use eframe::egui::{self, Color32, RichText};

use crate::audio::{Cmd, Engine, LiveCmd, Status};
use crate::edit;
use crate::library::Library;
use crate::midi::{self, Keyboard, Msg, Played, Target};
use crate::player::{REVERB_NAMES, Settings, preview};
use crate::prefs::Prefs;
use crate::rack::{RackAction, RackShow, RackView, meter};
use crate::roll::{self, RollView, bar_beat, channel_color, grid_name, key_name};
use crate::seq::{Note, NoteSpec, PPQN};
use crate::session::Session;

/// How the window starts (`--open` and friends).
#[derive(Debug, Default)]
pub struct Start {
    open: Option<String>,
    tab: Option<Tab>,
    seek: Option<u32>,
    paused: bool,
    master: Option<f32>,
    output: Option<f32>,
    channel: Option<usize>,
    hold_key: Option<u8>,
}

impl Start {
    pub fn parse(a: &[String]) -> Result<Start> {
        let mut s = Start::default();
        let mut i = 0;
        let val = |i: usize| a.get(i + 1).cloned().ok_or_else(|| anyhow!("{} needs a value", a[i]));
        while i < a.len() {
            match a[i].as_str() {
                "--open" => s.open = Some(val(i)?),
                "--tab" => {
                    s.tab = Some(match val(i)?.as_str() {
                        "rack" => Tab::Rack,
                        "roll" => Tab::Roll,
                        "mixer" => Tab::Mixer,
                        "chip" => Tab::Chip,
                        "song" => Tab::Song,
                        t => return Err(anyhow!("--tab rack|roll|mixer|chip|song, not {t}")),
                    })
                }
                "--seek" => s.seek = Some(val(i)?.parse()?),
                "--master" => s.master = Some(val(i)?.parse()?),
                "--output" => s.output = Some(val(i)?.parse()?),
                "--channel" => s.channel = Some(val(i)?.parse()?),
                "--hold-key" => s.hold_key = Some(val(i)?.parse()?),
                "--paused" => {
                    s.paused = true;
                    i += 1;
                    continue;
                }
                other => return Err(anyhow!("unknown argument {other}")),
            }
            i += 2;
        }
        Ok(s)
    }
}

pub fn run(config: Option<PathBuf>, start: Start) -> Result<()> {
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_inner_size([1440.0, 880.0]).with_min_inner_size([900.0, 560.0]).with_title("FFX Music Studio"),
        ..Default::default()
    };
    eframe::run_native("FFX Music Studio", opts, Box::new(move |cc| Ok(Box::new(App::new(cc, config, start))))).map_err(|e| anyhow!("{e}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tab {
    Rack,
    Roll,
    Mixer,
    Chip,
    Song,
}

#[derive(Default)]
struct Banks {
    selected: Option<u16>,
    wd: Option<Arc<Wd>>,
    inst: Option<usize>,
    key: u8,
    error: Option<String>,
}

/// Notes being recorded from the keyboard: the keys down (start tick,
/// velocity, channel) and the notes finished, not yet in the sequence.
#[derive(Default)]
struct Recording {
    on: bool,
    open: BTreeMap<u8, (u32, u8, usize)>,
    done: Vec<(usize, NoteSpec)>,
}

pub struct App {
    lib: Option<Library>,
    lib_rx: Option<Receiver<Result<Library>>>,
    lib_error: Option<String>,
    engine: Engine,
    session: Option<Session>,
    notes: Vec<Vec<Note>>,
    notes_version: Option<u64>,
    sent_version: u64,
    sent_settings: Option<Settings>,
    sent_wd: Option<Arc<Wd>>,
    filter: String,
    tab: Tab,
    roll: RollView,
    rack: RackView,
    banks: Banks,
    status: Status,
    message: Option<String>,
    seek_drag: Option<u32>,
    fit_pending: bool,
    start: Option<Start>,
    /// Scroll the roll here after fitting (`--seek`).
    scroll_to: Option<u32>,
    prefs: Prefs,
    prefs_path: PathBuf,
    saved_prefs: Prefs,
    /// `--output`: this run's output volume (not saved unless changed).
    cli_output: Option<f32>,
    sent_output: Option<f32>,
    sent_metronome: Option<bool>,
    keyboard: Arc<Mutex<Keyboard>>,
    midi_in: Option<midi::Input>,
    midi_ports: Vec<String>,
    midi_error: Option<String>,
    midi_tried: bool,
    rec: Recording,
    /// Keys lit on the strip: sounding live on a channel.
    lit: Vec<u8>,
    export_rx: Option<Receiver<Result<(PathBuf, f64)>>>,
    hold_key: Option<u8>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, config: Option<PathBuf>, mut start: Start) -> App {
        let ctx = cc.egui_ctx.clone();
        let engine = Engine::start(Box::new(move || ctx.request_repaint()));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Library::open(config.as_deref()));
            ctx.request_repaint();
        });
        let prefs_path = Prefs::path();
        let saved = Prefs::load(&prefs_path);
        let mut prefs = saved.clone();
        let cli_output = start.output.take();
        if let Some(o) = cli_output {
            prefs.output = o.clamp(0.0, 2.0);
        }
        let mut roll = RollView::default();
        roll.row_h = prefs.roll_row_h;
        roll.grid = prefs.grid;
        if prefs.roll_px_per_tick > 0.0 {
            roll.px_per_tick = prefs.roll_px_per_tick;
        }
        let mut rack = RackView::default();
        rack.row_h = prefs.rack_row_h;
        let hold_key = start.hold_key.take();
        App {
            lib: None,
            lib_rx: Some(rx),
            lib_error: None,
            engine,
            session: None,
            notes: Vec::new(),
            notes_version: None,
            sent_version: 0,
            sent_settings: None,
            sent_wd: None,
            filter: String::new(),
            tab: Tab::Rack,
            roll,
            rack,
            banks: Banks { key: 60, ..Banks::default() },
            status: Status::default(),
            message: None,
            seek_drag: None,
            fit_pending: false,
            start: Some(start),
            scroll_to: None,
            prefs,
            prefs_path,
            saved_prefs: saved,
            cli_output,
            sent_output: None,
            sent_metronome: None,
            keyboard: Arc::new(Mutex::new(Keyboard::default())),
            midi_in: None,
            midi_ports: Vec::new(),
            midi_error: None,
            midi_tried: false,
            rec: Recording::default(),
            lit: Vec::new(),
            export_rx: None,
            hold_key,
        }
    }

    fn load(&mut self, id: u16) {
        self.stop_recording();
        let Some(lib) = self.lib.as_mut() else { return };
        let Some(info) = lib.tracks.iter().find(|t| t.id == id).cloned() else { return };
        let r = lib.song(id).and_then(|(b, wd, missing)| Ok((Session::new(id, info.label(), &b, wd)?, missing)));
        match r {
            Ok((s, missing)) => {
                self.message = missing;
                let bgm = match s.bgm() {
                    Ok(b) => b,
                    Err(e) => {
                        self.message = Some(format!("{e:#}"));
                        return;
                    }
                };
                self.engine.send(Cmd::Load { tab: lib.tables.clone(), bgm, wd: s.wd.clone(), settings: s.settings() });
                self.engine.send(Cmd::Play);
                self.sent_version = s.version;
                self.sent_settings = Some(s.settings());
                self.sent_wd = Some(s.wd.clone());
                if self.banks.selected.is_none() {
                    self.banks.selected = Some(s.wd_orig.id);
                    self.banks.wd = lib.bank(s.wd_orig.id).ok();
                }
                self.session = Some(s);
                self.notes_version = None;
                self.fit_pending = true;
                self.rack.fit();
                if let Ok(mut k) = self.keyboard.lock() {
                    k.feed(Msg::AllOff);
                    k.played.clear();
                }
            }
            Err(e) => self.message = Some(format!("{}: {e:#}", info.label())),
        }
    }

    /// Hands the session's changes to the engine.
    fn sync(&mut self) {
        if self.sent_output != Some(self.prefs.output) {
            self.engine.send(Cmd::Output(self.prefs.output));
            self.sent_output = Some(self.prefs.output);
        }
        if self.sent_metronome != Some(self.prefs.metronome) {
            self.engine.send(Cmd::Metronome(self.prefs.metronome));
            self.sent_metronome = Some(self.prefs.metronome);
        }
        let Some(s) = &self.session else { return };
        if self.sent_wd.as_ref().is_none_or(|w| !Arc::ptr_eq(w, &s.wd)) {
            self.engine.send(Cmd::Wd(s.wd.clone()));
            self.sent_wd = Some(s.wd.clone());
        }
        if s.version != self.sent_version {
            match s.bgm() {
                Ok(b) => self.engine.send(Cmd::Bgm(b)),
                Err(e) => self.message = Some(format!("{e:#}")),
            }
            self.sent_version = s.version;
        }
        let st = s.settings();
        if self.sent_settings.as_ref() != Some(&st) {
            self.engine.send(Cmd::Settings(st.clone()));
            self.sent_settings = Some(st);
        }
    }

    fn refresh_notes(&mut self) {
        let Some(s) = &self.session else {
            self.notes.clear();
            return;
        };
        if self.notes_version != Some(s.version) {
            self.notes = (0..s.seq.tracks.len()).map(|t| s.seq.notes(t)).collect();
            self.notes_version = Some(s.version);
        }
    }

    /// The channel the keyboard plays on: the rack's or the roll's.
    fn channel(&self) -> usize {
        if self.tab == Tab::Roll { self.roll.active } else { self.rack.selected }
    }

    /// What a live note on channel `c` sounds like: its instrument now, its
    /// volume, pan and transpose (mute and solo do not silence it).
    fn live_mod(&self, c: usize) -> Option<TrackMod> {
        let s = self.session.as_ref()?;
        let ch = s.channels.get(c)?;
        let prog = self.status.programs.get(c).copied().flatten().filter(|_| ch.instrument.is_none()).or_else(|| s.program_at(c, self.status.tick));
        let prog = prog.or_else(|| (0..s.channels.len()).find_map(|o| s.program_at(o, 0))).unwrap_or(0);
        Some(TrackMod { gain: ch.gain, pan: ch.pan, transpose: ch.transpose, program: Some(prog) })
    }

    /// Notes from the keyboard strip and the MIDI keyboard: the target
    /// channel, what was played (recording, the lit keys).
    fn keyboard_frame(&mut self, strip: &[roll::Action]) {
        let c = self.channel();
        let target = self.live_mod(c).map(|m| Target { ch: c, m });
        let tick = self.status.tick;
        let grid = self.roll.grid.max(1);
        let recording = self.rec.on && self.status.playing;
        let Ok(mut k) = self.keyboard.lock() else { return };
        k.target = target;
        let mut cmds = Vec::new();
        for a in strip {
            match a {
                roll::Action::KeyOn(key) => cmds.extend(k.feed(Msg::On { key: *key, vel: self.roll.new_vel.max(1) })),
                roll::Action::KeyOff(key) => cmds.extend(k.feed(Msg::Off { key: *key })),
                _ => {}
            }
        }
        for p in std::mem::take(&mut k.played) {
            if !recording {
                continue;
            }
            match p {
                Played::On { ch, key, vel } => {
                    self.rec.open.insert(key, (tick, vel, ch));
                }
                Played::Off { key, .. } => {
                    if let Some((t0, vel, ch)) = self.rec.open.remove(&key) {
                        let start = edit::snap(t0 as f32, grid);
                        let end = edit::snap(tick as f32, grid).max(start + grid);
                        self.rec.done.push((ch, NoteSpec { start, len: end - start, key, vel }));
                    }
                }
            }
        }
        self.lit = k.sounding(c).collect();
        drop(k);
        for c in cmds {
            self.engine.send(Cmd::Live(c));
        }
    }

    /// Ends a recording: the notes played go into the sequence (one undo
    /// step).
    fn stop_recording(&mut self) {
        let tick = self.status.tick;
        let grid = self.roll.grid.max(1);
        for (key, (t0, vel, ch)) in std::mem::take(&mut self.rec.open) {
            let start = edit::snap(t0 as f32, grid);
            let end = edit::snap(tick as f32, grid).max(start + grid);
            self.rec.done.push((ch, NoteSpec { start, len: end - start, key, vel }));
        }
        self.rec.on = false;
        let done = std::mem::take(&mut self.rec.done);
        if done.is_empty() {
            return;
        }
        if let Some(s) = self.session.as_mut() {
            s.checkpoint();
            for (ch, spec) in &done {
                if *ch < s.seq.tracks.len() {
                    s.seq.add_note(*ch, *spec);
                }
            }
            s.seq_changed();
            s.checkpoint();
            self.message = Some(format!("recorded {} notes (Ctrl+Z takes them back)", done.len()));
        }
    }

    fn midi_frame(&mut self, ctx: &egui::Context) {
        if self.midi_tried {
            return;
        }
        self.midi_tried = true;
        match midi::ports() {
            Ok(p) => {
                self.midi_ports = p;
                self.midi_error = None;
            }
            Err(e) => {
                self.midi_ports.clear();
                self.midi_error = Some(e);
            }
        }
        if self.midi_in.is_some() || self.prefs.midi == "-" {
            return;
        }
        // the saved device, else the first one
        let want = if self.midi_ports.contains(&self.prefs.midi) { Some(self.prefs.midi.clone()) } else { self.midi_ports.first().cloned() };
        if let Some(name) = want {
            self.open_midi(ctx, &name);
        }
    }

    fn open_midi(&mut self, ctx: &egui::Context, name: &str) {
        self.midi_in = None;
        let c = ctx.clone();
        match midi::open(name, self.keyboard.clone(), self.engine.sender(), Box::new(move || c.request_repaint())) {
            Ok(i) => {
                self.midi_in = Some(i);
                self.midi_error = None;
            }
            Err(e) => self.midi_error = Some(e),
        }
    }

    fn track_list(&mut self, ui: &mut egui::Ui) {
        ui.heading("Tracks");
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("filter"));
        ui.separator();
        let Some(lib) = &self.lib else {
            if self.lib_error.is_none() {
                ui.label("Opening the disc...");
            }
            return;
        };
        let f = self.filter.to_lowercase();
        let rows: Vec<(u16, String, String)> = lib
            .tracks
            .iter()
            .filter(|t| f.is_empty() || t.label().to_lowercase().contains(&f) || t.pc_name.contains(&f))
            .map(|t| (t.id, if t.title.is_empty() { format!("music{:03}", t.id) } else { t.title.clone() }, t.pc_name.clone()))
            .collect();
        let current = self.session.as_ref().map(|s| s.id);
        let mut pick = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, 20.0, rows.len(), |ui, range| {
            for (id, title, pc) in &rows[range] {
                let text = RichText::new(title.to_string()).strong();
                let r = ui
                    .horizontal(|ui| {
                        let r = ui.selectable_label(current == Some(*id), text);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.label(RichText::new(format!("{id:03}")).monospace().weak());
                        });
                        r
                    })
                    .inner;
                let r = if pc.is_empty() { r } else { r.on_hover_text(pc) };
                if r.clicked() {
                    pick = Some(*id);
                }
            }
        });
        if let Some(id) = pick {
            self.load(id);
        }
    }

    fn play_from(&mut self, tick: u32) {
        self.engine.send(Cmd::SeekTick(tick));
        self.engine.send(Cmd::Play);
    }

    fn transport(&mut self, ui: &mut egui::Ui) {
        let st = self.status.clone();
        let Some(s) = self.session.as_mut() else {
            ui.horizontal(|ui| {
                ui.heading("FFX Music Studio");
                if let Some(lib) = &self.lib {
                    ui.label(RichText::new(format!("disc {} (settings: {})", lib.iso.display(), lib.config_path.display())).weak());
                }
            });
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("sound output: {}", self.engine.device)).weak());
                ui.separator();
                ui.label("Output");
                ui.add(egui::Slider::new(&mut self.prefs.output, 0.0..=2.0).fixed_decimals(2));
            });
            return;
        };
        let end = s.seq.end_tick();
        let mut cmds: Vec<Cmd> = Vec::new();
        let mut export = false;
        let mut rec_toggle = false;
        let mut pick: Option<Option<String>> = None;
        ui.horizontal(|ui| {
            ui.heading(&s.label);
            if s.edited() {
                ui.label(RichText::new("edited").color(Color32::from_rgb(255, 200, 90)));
            }
            ui.separator();
            if ui.add_enabled(s.can_undo(), egui::Button::new("Undo")).on_hover_text("Ctrl+Z").clicked() {
                s.undo();
            }
            if ui.add_enabled(s.can_redo(), egui::Button::new("Redo")).on_hover_text("Ctrl+Y").clicked() {
                s.redo();
            }
            ui.separator();
            export = ui
                .add_enabled(self.export_rx.is_none(), egui::Button::new(if self.export_rx.is_some() { "Exporting..." } else { "Export WAV" }))
                .on_hover_text("This track as edited, once through, as a 48 kHz WAV in the exports folder next to the exe")
                .clicked();
        });
        ui.horizontal(|ui| {
            let playing = st.playing;
            if ui.add_sized([70.0, 24.0], egui::Button::new(if playing { "Pause" } else { "Play" })).on_hover_text("Space").clicked() {
                cmds.push(if playing { Cmd::Pause } else { Cmd::Play });
            }
            if ui.add_sized([60.0, 24.0], egui::Button::new("Stop")).on_hover_text("Back to the start (the notes fade out; again: cut)").clicked() {
                cmds.push(Cmd::Stop);
                // and any live note left sounding
                cmds.push(Cmd::Live(LiveCmd::AllOff));
            }
            let (a, _) = s.markers();
            if ui.add_sized([70.0, 24.0], egui::Button::new("Play A-B")).on_hover_text("Play the loop from A, around between the markers").clicked() {
                s.looping = true;
                cmds.push(Cmd::SeekTick(a));
                cmds.push(Cmd::Play);
            }
            ui.checkbox(&mut s.looping, "Loop").on_hover_text("Jump back at the end (the song's loop point, or the A-B markers); off: play to the end and stop");
            ui.toggle_value(&mut self.prefs.metronome, "Metronome").on_hover_text("A click on every beat (higher on the bar)");
            let rec = RichText::new(if self.rec.on { "Rec \u{2022}" } else { "Rec" }).color(if self.rec.on { Color32::from_rgb(255, 90, 90) } else { Color32::from_gray(200) });
            rec_toggle = ui.add(egui::Button::new(rec).selected(self.rec.on)).on_hover_text("Record what you play (MIDI or the piano keys) into the selected channel while the song plays, snapped to the grid").clicked();
            ui.separator();
            let secs = st.frame as f64 / 48000.0;
            ui.label(RichText::new(format!("{:02}:{:05.2}", (secs / 60.0) as u32, secs % 60.0)).monospace().size(18.0));
            ui.label(RichText::new(format!("bar {}  tick {}", bar_beat(st.tick), st.tick)).monospace());
            ui.separator();
            let mut t = self.seek_drag.unwrap_or(st.tick.min(end));
            let r = ui.add(egui::Slider::new(&mut t, 0..=end.max(1)).show_value(false).trailing_fill(true)).on_hover_text("Seek (ticks of the sequence)");
            if r.dragged() {
                self.seek_drag = Some(t);
            }
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                cmds.push(Cmd::SeekTick(t));
                self.seek_drag = None;
            }
            ui.label(format!("/ {} ({:.0} s)", bar_beat(end), s.seq.seconds_at(end)));
        });
        ui.horizontal(|ui| {
            ui.label("Tempo");
            ui.add(egui::Slider::new(&mut s.tempo, 0.25..=2.0).logarithmic(true).fixed_decimals(2).suffix("x")).on_hover_text("Sequencer tempo (pitch stays)");
            // before its first tempo command the driver runs at 1 BPM
            let bpm = if st.bpm > 1 { f64::from(st.bpm) } else { s.seq.tempos().first().map_or(1.0, |t| f64::from(t.3)) };
            ui.label(format!("{} BPM", (bpm * s.tempo).round()));
            ui.separator();
            ui.label("Speed");
            ui.add(egui::Slider::new(&mut s.speed, 0.5..=2.0).logarithmic(true).fixed_decimals(2).suffix("x")).on_hover_text("Playback speed (pitch goes with it)");
            ui.separator();
            ui.label("Output");
            ui.add(egui::Slider::new(&mut self.prefs.output, 0.0..=2.0).fixed_decimals(2)).on_hover_text("Output volume of the studio (after everything; kept for every track and the next start). The track's own volume is in Sound chip.");
            ui.separator();
            if ui.button("Reset track").on_hover_text("Back to the track as on the disc: notes, mixer, instruments, chip, loop, tempo, track volume (Ctrl+Z undoes it)").clicked() {
                s.reset();
                cmds.push(Cmd::Stop);
                cmds.push(Cmd::Stop);
                cmds.push(Cmd::Play);
                self.roll.selected.clear();
            }
            ui.separator();
            ui.label("MIDI in");
            let current = self.midi_in.as_ref().map_or("none".to_string(), |m| m.name.clone());
            egui::ComboBox::from_id_salt("midi").width(160.0).selected_text(current).show_ui(ui, |ui| {
                if ui.selectable_label(self.midi_in.is_none(), "none").clicked() {
                    pick = Some(None);
                }
                for p in &self.midi_ports {
                    if ui.selectable_label(self.midi_in.as_ref().is_some_and(|m| &m.name == p), p).clicked() {
                        pick = Some(Some(p.clone()));
                    }
                }
            });
            if ui.small_button("rescan").on_hover_text("Look for MIDI devices again").clicked() {
                self.midi_tried = false;
            }
            if let Some(e) = &self.midi_error {
                ui.label(RichText::new(e).weak().small());
            } else if self.midi_ports.is_empty() {
                ui.label(RichText::new("no MIDI device").weak().small());
            }
            ui.separator();
            let state = if st.fading { "  fading out" } else { "" };
            let busy = format!("voices {:2}/48{state}   {}", st.active_voices, if st.busy_ms > 0 { format!("re-sequence {} ms", st.busy_ms) } else { String::new() });
            ui.label(RichText::new(busy).monospace().weak());
        });
        if let Some(p) = pick {
            match p {
                None => {
                    self.midi_in = None;
                    self.prefs.midi = "-".into();
                }
                Some(name) => {
                    self.prefs.midi = name.clone();
                    let c = ui.ctx().clone();
                    self.open_midi(&c, &name);
                }
            }
        }
        if rec_toggle {
            if self.rec.on {
                self.stop_recording();
            } else {
                self.rec.on = true;
                if !st.playing {
                    cmds.push(Cmd::Play);
                }
            }
        }
        if export {
            self.start_export();
        }
        for c in cmds {
            self.engine.send(c);
        }
    }

    fn start_export(&mut self) {
        let (Some(s), Some(lib)) = (&self.session, &self.lib) else { return };
        let bgm = match s.bgm() {
            Ok(b) => b,
            Err(e) => {
                self.message = Some(format!("{e:#}"));
                return;
            }
        };
        let name: String = s.label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let path = demaster::build::exe_dir().join("exports").join(format!("music{:03}-{}-{stamp}.wav", s.id, name.trim_matches('_')));
        let (tab, wd, settings) = (lib.tables.clone(), s.wd.clone(), s.settings());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let r = crate::player::export_wav(tab, bgm, wd, settings, &path, 1200.0).map(|secs| (path, secs));
            let _ = tx.send(r);
        });
        self.export_rx = Some(rx);
    }

    fn rack_tab(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.session.as_mut() else { return };
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.rack.follow, "follow");
            ui.label(RichText::new("click: select the channel (the keyboard plays it) \u{b7} double click: its piano roll \u{b7} ruler: play from there, drag A/B \u{b7} wheel: rows, Shift: time, Ctrl: zoom time, Alt: row height").weak());
        });
        let head = self.status.loaded.then_some(self.seek_drag.unwrap_or(self.status.tick));
        let programs: Vec<Option<usize>> = (0..s.channels.len()).map(|c| self.status.programs.get(c).copied().flatten().or_else(|| s.program_at(c, 0))).collect();
        let saved = self.rack.follow;
        self.rack.follow &= self.status.playing;
        let actions = self.rack.show(ui, s, RackShow { notes: &self.notes, playhead: head, levels: &self.status.levels, programs: &programs });
        self.rack.follow = saved;
        for a in actions {
            match a {
                RackAction::Seek(t) => self.play_from(t),
                RackAction::Loop(a, b) => {
                    if let Some(s) = self.session.as_mut() {
                        s.set_loop(Some((a, b)));
                        s.looping = true;
                    }
                }
                RackAction::Open(c) => {
                    self.roll.focus(c);
                    self.tab = Tab::Roll;
                }
            }
        }
        self.roll.active = self.rack.selected;
    }

    fn roll_tab(&mut self, ui: &mut egui::Ui) -> Vec<roll::Action> {
        let Some(s) = self.session.as_mut() else { return Vec::new() };
        let n = s.seq.tracks.len();
        let mut delete = false;
        ui.horizontal(|ui| {
            ui.label("Channel");
            egui::ComboBox::from_id_salt("active").width(150.0).selected_text(chan_label(self.roll.active, &self.notes)).show_ui(ui, |ui| {
                for c in 0..n {
                    ui.selectable_value(&mut self.roll.active, c, chan_label(c, &self.notes));
                }
            });
            ui.checkbox(&mut self.roll.show_all, "all channels");
            ui.checkbox(&mut self.roll.follow, "follow");
            ui.separator();
            ui.label("Snap");
            egui::ComboBox::from_id_salt("grid").width(60.0).selected_text(grid_name(self.roll.grid)).show_ui(ui, |ui| {
                for g in [PPQN * 4, PPQN, PPQN / 2, PPQN / 4, PPQN / 8, PPQN / 3, PPQN / 6, 1] {
                    ui.selectable_value(&mut self.roll.grid, g, grid_name(g));
                }
            });
            ui.label("Zoom");
            ui.add(egui::Slider::new(&mut self.roll.px_per_tick, 0.01..=16.0).logarithmic(true).show_value(false)).on_hover_text("Ctrl+wheel");
            ui.add(egui::Slider::new(&mut self.roll.row_h, 3.0..=40.0).show_value(false)).on_hover_text("Key height (Alt+wheel)");
            ui.separator();
            let chan = self.notes.get(self.roll.active);
            let sel: Vec<Note> = chan.map_or(Vec::new(), |ns| ns.iter().filter(|x| self.roll.selected.contains(&x.id)).copied().collect());
            match sel.as_slice() {
                [note] => {
                    let c = self.roll.active;
                    let mut spec = edit::spec(note);
                    ui.label(RichText::new(format!("note {}", key_name(note.key))).strong());
                    let mut changed = false;
                    changed |= ui.add(egui::DragValue::new(&mut spec.key).range(0..=127).custom_formatter(|v, _| key_name(v as u8))).changed();
                    ui.label("start");
                    changed |= ui.add(egui::DragValue::new(&mut spec.start).range(0..=s.seq.end_tick())).changed();
                    ui.label("len");
                    changed |= ui.add(egui::DragValue::new(&mut spec.len).range(1..=100_000)).changed();
                    ui.label("vel");
                    changed |= ui.add(egui::DragValue::new(&mut spec.vel).range(1..=127)).changed();
                    if changed {
                        s.seq.set_note(c, note.id, Some(spec));
                        s.seq_changed();
                    }
                    delete = ui.button("Delete").clicked();
                }
                [] => {
                    ui.label(RichText::new("click: add \u{b7} drag: move / box select \u{b7} Ctrl/Shift+click: add to selection \u{b7} Del \u{b7} Ctrl+C/V \u{b7} arrows: nudge \u{b7} keys on the left: listen").weak());
                }
                many => {
                    ui.label(RichText::new(format!("{} notes selected", many.len())).strong());
                    delete = ui.button("Delete").clicked();
                }
            }
        });
        if delete {
            let c = self.roll.active;
            for id in std::mem::take(&mut self.roll.selected) {
                s.seq.set_note(c, id, None);
            }
            s.seq_changed();
        }
        self.refresh_notes();
        let Some(s) = self.session.as_mut() else { return Vec::new() };
        if self.fit_pending {
            self.roll.fit(&self.notes, ui.available_width(), self.prefs.roll_px_per_tick > 0.0);
            self.rack.selected = self.roll.active;
            self.fit_pending = false;
            // the toolbar above was drawn before: once more
            ui.ctx().request_repaint();
            if let Some(t) = self.scroll_to.take() {
                self.roll.scroll_tick = t.saturating_sub(PPQN * 4) as f32;
            }
        }
        let head = self.status.loaded.then_some(self.seek_drag.unwrap_or(self.status.tick));
        let saved = self.roll.follow;
        self.roll.follow &= self.status.playing;
        let ghosts: Vec<NoteSpec> = self.rec.done.iter().filter(|(c, _)| *c == self.roll.active).map(|x| x.1).collect();
        let actions = self.roll.show(ui, s, roll::Show { notes: &self.notes, playhead: head, lit: &self.lit, ghosts: &ghosts });
        self.roll.follow = saved;
        let mut strip = Vec::new();
        for a in actions {
            match a {
                roll::Action::Edited => s.seq_changed(),
                roll::Action::Seek(t) => {
                    self.engine.send(Cmd::SeekTick(t));
                    self.engine.send(Cmd::Play);
                }
                roll::Action::Loop(a, b) => {
                    s.set_loop(Some((a, b)));
                    s.looping = true;
                }
                k @ (roll::Action::KeyOn(_) | roll::Action::KeyOff(_)) => strip.push(k),
            }
        }
        self.rack.selected = self.roll.active;
        strip
    }

    fn mixer_tab(&mut self, ui: &mut egui::Ui) {
        let st = self.status.clone();
        let Some(mut session) = self.session.take() else { return };
        let s = &mut session;
        let picked = match (&self.banks.wd, self.banks.inst, self.banks.selected) {
            (Some(w), Some(i), Some(b)) => Some((b, w.clone(), i)),
            _ => None,
        };
        let total = ui.available_width();
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width((total - 360.0).max(500.0));
                ui.heading("Mixer");
                egui::ScrollArea::vertical().id_salt("mixer").auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new("mix").striped(true).spacing([10.0, 6.0]).show(ui, |ui| {
                        for h in ["", "ch", "level", "voices", "program", "mute", "solo", "volume", "pan", "transpose", "instrument", ""] {
                            ui.label(RichText::new(h).strong());
                        }
                        ui.end_row();
                        for c in 0..s.channels.len() {
                            let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 16.0), egui::Sense::hover());
                            ui.painter().rect_filled(r, 2.0, channel_color(c));
                            ui.label(format!("{c}"));
                            // the activity: a LED and a meter
                            let m = meter(st.levels.get(c).copied().unwrap_or(0.0));
                            let (r, _) = ui.allocate_exact_size(egui::vec2(70.0, 14.0), egui::Sense::hover());
                            let led = egui::pos2(r.left() + 6.0, r.center().y);
                            ui.painter().circle_filled(led, 5.0, if m > 0.02 { channel_color(c).gamma_multiply(0.35 + 0.65 * m) } else { Color32::from_gray(45) });
                            let bar = egui::Rect::from_min_max(egui::pos2(r.left() + 16.0, r.top() + 3.0), egui::pos2(r.right(), r.bottom() - 3.0));
                            ui.painter().rect_filled(bar, 2.0, Color32::from_gray(40));
                            let mut f = bar;
                            f.set_width(bar.width() * m);
                            ui.painter().rect_filled(f, 2.0, channel_color(c));
                            let v = st.voices.get(c).copied().unwrap_or(0);
                            ui.label(format!("{v}"));
                            let prog = st.programs.get(c).copied().flatten();
                            let has = s.seq.has_notes(c);
                            ui.label(match prog {
                                Some(p) => format!("{p}"),
                                None if has => "-".into(),
                                None => "(no notes)".into(),
                            });
                            let ch = &mut s.channels[c];
                            ui.toggle_value(&mut ch.mute, "M");
                            ui.toggle_value(&mut ch.solo, "S");
                            ui.add(egui::Slider::new(&mut ch.gain, 0.0..=2.0).fixed_decimals(2));
                            ui.add(egui::Slider::new(&mut ch.pan, -64..=64));
                            ui.add(egui::DragValue::new(&mut ch.transpose).range(-36..=36).suffix(" st"));
                            match ch.instrument {
                                Some((b, i, _)) => ui.label(format!("bank {b} #{i}")),
                                None => ui.label(RichText::new("own").weak()),
                            };
                            ui.horizontal(|ui| {
                                let r = ui.add_enabled(picked.is_some(), egui::Button::new("use selected")).on_hover_text("Play this channel with the instrument selected on the right");
                                if r.clicked()
                                    && let Some((b, w, i)) = &picked
                                {
                                    s.set_instrument(c, Some((*b, w, *i)));
                                }
                                if s.channels[c].instrument.is_some() && ui.small_button("x").on_hover_text("Back to the track's own instrument").clicked() {
                                    s.set_instrument(c, None);
                                }
                            });
                            ui.end_row();
                        }
                    });
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                ui.set_width(340.0);
                self.bank_browser(ui, Some(s.wd_orig.id));
            });
        });
        self.session = Some(session);
    }

    fn bank_browser(&mut self, ui: &mut egui::Ui, own: Option<u16>) {
        ui.heading("Instruments");
        let Some(lib) = self.lib.as_mut() else { return };
        let label = |b: u16| if Some(b) == own { format!("bank {b} (this track)") } else { format!("bank {b}") };
        let mut pick = None;
        egui::ComboBox::from_id_salt("bank").width(200.0).selected_text(self.banks.selected.map_or("bank".into(), label)).show_ui(ui, |ui| {
            for &b in &lib.banks {
                if ui.selectable_label(self.banks.selected == Some(b), label(b)).clicked() {
                    pick = Some(b);
                }
            }
        });
        if let Some(b) = pick {
            self.banks.selected = Some(b);
            self.banks.inst = None;
            match lib.bank(b) {
                Ok(w) => {
                    self.banks.wd = Some(w);
                    self.banks.error = None;
                }
                Err(e) => {
                    self.banks.wd = None;
                    self.banks.error = Some(format!("{e:#}"));
                }
            }
        }
        if let Some(e) = &self.banks.error {
            ui.colored_label(Color32::LIGHT_RED, e);
        }
        ui.horizontal(|ui| {
            ui.label("preview key");
            ui.add(egui::DragValue::new(&mut self.banks.key).range(0..=127).custom_formatter(|v, _| key_name(v as u8)));
        });
        let Some(w) = self.banks.wd.clone() else { return };
        ui.label(RichText::new(format!("{} instruments, {} KB of samples", w.instruments.len(), w.samples.len() / 1024)).weak());
        let mut play = None;
        egui::ScrollArea::vertical().id_salt("insts").auto_shrink([false, false]).show_rows(ui, 22.0, w.instruments.len(), |ui, range| {
            for i in range {
                let regs = w.instrument_regions(i);
                let (lo, hi) = regs.iter().fold((127i8, 0i8), |(a, b), r| (a.min(r.key_hi), b.max(r.key_hi)));
                ui.horizontal(|ui| {
                    if ui.small_button("Play").clicked() {
                        play = Some(i);
                    }
                    let splits = if regs.len() > 1 { format!(", splits {}..{}", key_name(lo.max(0) as u8), key_name(hi.max(0) as u8)) } else { String::new() };
                    let text = format!("#{i:<3} {} region{}{splits}", regs.len(), if regs.len() == 1 { "" } else { "s" });
                    if ui.selectable_label(self.banks.inst == Some(i), text).clicked() {
                        self.banks.inst = Some(i);
                    }
                });
            }
        });
        if let Some(i) = play {
            self.banks.inst = Some(i);
            let snd = preview(&lib.tables, &w, i, self.banks.key, 2);
            self.engine.send(Cmd::Preview(snd));
        }
    }

    fn chip_tab(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.session.as_mut() else { return };
        let song_mode = s.seq.header[9];
        let song_name = REVERB_NAMES.get(usize::from(song_mode)).copied().unwrap_or("none");
        ui.heading("Reverb");
        ui.horizontal(|ui| {
            ui.label("preset");
            let text = match s.chip.reverb {
                None => format!("the song's ({song_name})"),
                Some(m) => format!("{m}: {}", REVERB_NAMES[usize::from(m)]),
            };
            egui::ComboBox::from_id_salt("rev").width(200.0).selected_text(text).show_ui(ui, |ui| {
                ui.selectable_value(&mut s.chip.reverb, None, format!("the song's ({song_name})"));
                for (m, n) in REVERB_NAMES.iter().enumerate() {
                    ui.selectable_value(&mut s.chip.reverb, Some(m as u8), format!("{m}: {n}"));
                }
            });
        });
        ui.label(RichText::new("libsd's presets as read from the disc (the work area restarts silent when switched)").weak());
        egui::Grid::new("chip").spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("wet (reverb)");
            ui.add(egui::Slider::new(&mut s.chip.wet, 0.0..=2.0).fixed_decimals(2));
            ui.end_row();
            ui.label("dry");
            ui.add(egui::Slider::new(&mut s.chip.dry, 0.0..=2.0).fixed_decimals(2));
            ui.end_row();
        });
        ui.separator();
        ui.heading("Interpolation");
        ui.horizontal(|ui| {
            ui.radio_value(&mut s.chip.interp, Interp::Gaussian, "Gaussian (the SPU2's 4-tap)");
            ui.radio_value(&mut s.chip.interp, Interp::Linear, "linear");
            ui.radio_value(&mut s.chip.interp, Interp::Nearest, "nearest (none)");
        });
        ui.separator();
        ui.heading("Volume");
        egui::Grid::new("vols").spacing([12.0, 8.0]).show(ui, |ui| {
            ui.label("Track volume");
            ui.add(egui::Slider::new(&mut s.master, 0.0..=2.0).fixed_decimals(2)).on_hover_text("This track's level before the limiter (a track edit: back to 1.00 with Reset track or another track)");
            ui.end_row();
            ui.label("Output volume");
            ui.add(egui::Slider::new(&mut self.prefs.output, 0.0..=2.0).fixed_decimals(2)).on_hover_text("The studio's playback volume after everything (kept for every track and the next start)");
            ui.end_row();
        });
        ui.label(RichText::new("After the chip: the track volume, +9 dB and a -1 dBFS limiter, as the converter's music bank (docs/music.md); then the output volume.").weak());
        ui.label(RichText::new(format!("sound output: {}", self.engine.device)).weak());
    }

    fn song_tab(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.session.as_mut() else { return };
        let h = &s.seq.header;
        let (song_a, song_b) = s.song_loop();
        egui::Grid::new("info").spacing([16.0, 4.0]).show(ui, |ui| {
            let row = |ui: &mut egui::Ui, k: &str, v: String| {
                ui.label(RichText::new(k).strong());
                ui.label(v);
                ui.end_row();
            };
            row(ui, "sequence", format!("music{:03}.bgm, {} bytes", s.id, s.orig.encode().len()));
            let n = s.wd_orig.instruments.len();
            row(ui, "instrument bank", format!("WD {} ({n} instrument{})", u16::from_le_bytes([h[6], h[7]]), if n == 1 { "" } else { "s" }));
            row(ui, "channels", format!("{}", s.seq.tracks.len()));
            row(ui, "reverb", format!("mode {} ({}), depth {}", h[9], REVERB_NAMES.get(usize::from(h[9])).copied().unwrap_or("none"), i16::from_le_bytes([h[10], h[11]])));
            row(ui, "volume", format!("{}", h[12] & 0x7f));
            row(ui, "length", format!("{} ticks, bar {}, {:.1} s", s.seq.end_tick(), bar_beat(s.seq.end_tick()), s.seq.seconds_at(s.seq.end_tick())));
            row(ui, "song's loop", match song_a {
                Some(a) => format!("from tick {a} (bar {}) to {song_b} (bar {})", bar_beat(a), bar_beat(song_b)),
                None => "none (one-shot)".into(),
            });
        });
        ui.separator();
        ui.heading("Loop markers");
        let (mut a, mut b) = s.markers();
        let end = s.seq.end_tick();
        ui.horizontal(|ui| {
            ui.label("A");
            let ra = ui.add(egui::DragValue::new(&mut a).range(0..=end).speed(4.0));
            ui.label(bar_beat(a));
            ui.label("B");
            let rb = ui.add(egui::DragValue::new(&mut b).range(1..=end + 1).speed(4.0));
            ui.label(bar_beat(b));
            if ra.changed() || rb.changed() {
                s.set_loop(Some((a, b)));
                s.looping = true;
            }
            if ui.add_enabled(s.loop_range.is_some(), egui::Button::new("song's loop")).clicked() {
                s.set_loop(None);
            }
        });
        ui.label(RichText::new("Or drag the A and B markers in the ruler (channel rack or piano roll). A studio loop restores the whole sequencer state of tick A (volumes and programs too); the song's own loop restores what the PS2 driver does.").weak());
        ui.separator();
        ui.heading("Tempo commands");
        let tempos = s.seq.tempos();
        let mut set = None;
        egui::ScrollArea::vertical().id_salt("tempos").max_height(300.0).show(ui, |ui| {
            egui::Grid::new("tempos").striped(true).show(ui, |ui| {
                ui.label(RichText::new("tick").strong());
                ui.label(RichText::new("bar").strong());
                ui.label(RichText::new("channel").strong());
                ui.label(RichText::new("BPM").strong());
                ui.end_row();
                for (k, &(t, e, tick, bpm)) in tempos.iter().enumerate() {
                    ui.label(format!("{tick}"));
                    ui.label(bar_beat(tick));
                    ui.label(format!("{t}"));
                    let mut v = bpm;
                    if ui.push_id(k, |ui| ui.add(egui::DragValue::new(&mut v).range(1..=255))).inner.changed() {
                        set = Some((t, e, v));
                    }
                    ui.end_row();
                }
            });
        });
        if let Some((t, e, v)) = set {
            s.seq.set_tempo(t, e, v);
            s.seq_changed();
        }
    }

    /// Window settings into the file when they changed (not mid-drag).
    fn save_prefs(&mut self, idle: bool) {
        self.prefs.roll_row_h = self.roll.row_h;
        self.prefs.roll_px_per_tick = self.roll.px_per_tick;
        self.prefs.rack_row_h = self.rack.row_h;
        self.prefs.grid = self.roll.grid;
        let mut p = self.prefs.clone();
        // `--output` is for this run unless moved
        if self.cli_output == Some(p.output) {
            p.output = self.saved_prefs.output;
        }
        if idle && p != self.saved_prefs {
            if p.save(&self.prefs_path).is_ok() {
                self.saved_prefs = p;
            }
        }
    }
}

fn chan_label(c: usize, notes: &[Vec<Note>]) -> String {
    let n = notes.get(c).map_or(0, Vec::len);
    format!("{c} ({n} notes)")
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        if let Some(rx) = &self.lib_rx
            && let Ok(r) = rx.try_recv()
        {
            match r {
                Ok(l) => self.lib = Some(l),
                Err(e) => self.lib_error = Some(format!("{e:#}")),
            }
            self.lib_rx = None;
            if let (Some(st), Some(lib)) = (self.start.take(), &self.lib) {
                if let Some(t) = st.tab {
                    self.tab = t;
                }
                match st.open.as_deref().map(|q| lib.find(q).map(|t| t.id)) {
                    Some(Some(id)) => {
                        self.load(id);
                        if let Some(s) = self.session.as_mut()
                            && let Some(m) = st.master
                        {
                            s.master = m;
                            s.checkpoint();
                        }
                        self.sync();
                        if st.paused {
                            self.engine.send(Cmd::Pause);
                        }
                        if let Some(t) = st.seek {
                            self.engine.send(Cmd::SeekTick(t));
                            self.scroll_to = Some(t);
                        }
                        if let Some(c) = st.channel {
                            self.refresh_notes();
                            self.roll.fit(&self.notes, 1100.0, self.prefs.roll_px_per_tick > 0.0);
                            self.fit_pending = false;
                            if let Some(t) = self.scroll_to.take() {
                                self.roll.scroll_tick = t.saturating_sub(PPQN * 4) as f32;
                            }
                            self.roll.active = c;
                            self.rack.selected = c;
                            if self.tab == Tab::Roll {
                                self.roll.show_all = false;
                            }
                        }
                    }
                    Some(None) => self.message = Some(format!("--open: no track {}", st.open.unwrap_or_default())),
                    None => {}
                }
            }
        }
        self.midi_frame(ui.ctx());
        if let Some(rx) = &self.export_rx
            && let Ok(r) = rx.try_recv()
        {
            self.message = Some(match r {
                Ok((p, secs)) => format!("exported {secs:.1} s to {}", p.display()),
                Err(e) => format!("export: {e:#}"),
            });
            self.export_rx = None;
        }
        self.status = self.engine.status();
        if self.rec.on && !self.status.playing {
            self.stop_recording();
        }
        let typing = ui.ctx().egui_wants_keyboard_input();
        if !typing && self.session.is_some() {
            let (space, undo, redo) = ui.input(|i| {
                let cmd = i.modifiers.command;
                (
                    i.key_pressed(egui::Key::Space),
                    cmd && !i.modifiers.shift && i.key_pressed(egui::Key::Z),
                    cmd && (i.key_pressed(egui::Key::Y) || i.modifiers.shift && i.key_pressed(egui::Key::Z)),
                )
            });
            if space {
                self.engine.send(if self.status.playing { Cmd::Pause } else { Cmd::Play });
            }
            if let Some(s) = self.session.as_mut() {
                if undo {
                    s.undo();
                }
                if redo {
                    s.redo();
                }
            }
        }
        egui::Panel::left("tracks").resizable(true).default_size(280.0).show(ui, |ui| self.track_list(ui));
        egui::Panel::top("transport").show(ui, |ui| {
            ui.add_space(4.0);
            self.transport(ui);
            ui.add_space(2.0);
        });
        if let Some(m) = self.message.clone() {
            egui::Panel::bottom("message").show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.colored_label(Color32::from_rgb(255, 200, 90), m);
                    if ui.small_button("x").clicked() {
                        self.message = None;
                    }
                });
            });
        }
        let mut strip = Vec::new();
        egui::CentralPanel::default().show(ui, |ui| {
            if let Some(e) = &self.lib_error {
                ui.add_space(40.0);
                ui.heading("FFX Music Studio cannot open your disc");
                ui.add_space(10.0);
                ui.label(RichText::new(e).size(15.0));
                ui.add_space(10.0);
                ui.label("FFX Music Studio reads the same demaster.toml as demaster.exe: `iso` under [source] is your PS2 disc image of Final Fantasy X, `game` the folder with FFX.exe (for the disc's file names). Start it with --config <file> to use another one.");
                return;
            }
            if self.session.is_none() {
                ui.add_space(40.0);
                ui.heading(if self.lib.is_some() { "Pick a track on the left." } else { "Opening the disc..." });
                return;
            }
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Rack, RichText::new("Channel rack").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Roll, RichText::new("Piano roll").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Mixer, RichText::new("Mixer & instruments").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Chip, RichText::new("Sound chip").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Song, RichText::new("Song: loop & tempo").size(15.0));
            });
            ui.separator();
            self.refresh_notes();
            match self.tab {
                Tab::Rack => self.rack_tab(ui),
                Tab::Roll => strip = self.roll_tab(ui),
                Tab::Mixer => self.mixer_tab(ui),
                Tab::Chip => self.chip_tab(ui),
                Tab::Song => self.song_tab(ui),
            }
        });
        // `--hold-key`: a key held on the strip from the start (screenshots)
        if let Some(k) = self.hold_key
            && self.session.is_some()
            && self.status.loaded
            && self.lit.is_empty()
        {
            strip.push(roll::Action::KeyOn(k));
            self.hold_key = None;
        }
        self.keyboard_frame(&strip);
        let idle = !ui.input(|i| i.pointer.any_down());
        if idle && let Some(s) = self.session.as_mut() {
            s.checkpoint();
        }
        self.save_prefs(idle);
        self.sync();
        if self.status.playing || self.status.fading || !self.lit.is_empty() || self.export_rx.is_some() || self.rec.on {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        }
    }
}
