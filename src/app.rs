//! The window: the track list, the transport, and the tabs (piano roll,
//! mixer and instruments, sound chip, song). Edits go into the [`Session`];
//! [`App::sync`] hands what changed to the audio engine.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

use anyhow::{Result, anyhow};
use demaster::spu2::Interp;
use demaster::sqseq::Wd;
use eframe::egui::{self, Color32, RichText};

use crate::audio::{Cmd, Engine, Status};
use crate::library::Library;
use crate::player::{REVERB_NAMES, Settings, preview};
use crate::roll::{self, RollView, bar_beat, channel_color, key_name};
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
                        "roll" => Tab::Roll,
                        "mixer" => Tab::Mixer,
                        "chip" => Tab::Chip,
                        "song" => Tab::Song,
                        t => return Err(anyhow!("--tab roll|mixer|chip|song, not {t}")),
                    })
                }
                "--seek" => s.seek = Some(val(i)?.parse()?),
                "--master" => s.master = Some(val(i)?.parse()?),
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
    banks: Banks,
    status: Status,
    message: Option<String>,
    seek_drag: Option<u32>,
    fit_pending: bool,
    start: Option<Start>,
    /// Scroll the roll here after fitting (`--seek`).
    scroll_to: Option<u32>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, config: Option<PathBuf>, start: Start) -> App {
        let ctx = cc.egui_ctx.clone();
        let engine = Engine::start(Box::new(move || ctx.request_repaint()));
        let (tx, rx) = std::sync::mpsc::channel();
        let ctx = cc.egui_ctx.clone();
        std::thread::spawn(move || {
            let _ = tx.send(Library::open(config.as_deref()));
            ctx.request_repaint();
        });
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
            tab: Tab::Roll,
            roll: RollView::default(),
            banks: Banks { key: 60, ..Banks::default() },
            status: Status::default(),
            message: None,
            seek_drag: None,
            fit_pending: false,
            start: Some(start),
            scroll_to: None,
        }
    }

    fn load(&mut self, id: u16) {
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
            }
            Err(e) => self.message = Some(format!("{}: {e:#}", info.label())),
        }
    }

    /// Hands the session's changes to the engine.
    fn sync(&mut self) {
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
                let text = RichText::new(format!("{title}")).strong();
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

    fn transport(&mut self, ui: &mut egui::Ui) {
        let st = self.status.clone();
        let Some(s) = self.session.as_mut() else {
            ui.horizontal(|ui| {
                ui.heading("FFX Music Studio");
                if let Some(lib) = &self.lib {
                    ui.label(RichText::new(format!("disc {} (settings: {})", lib.iso.display(), lib.config_path.display())).weak());
                }
            });
            ui.label(RichText::new(format!("sound output: {}", self.engine.device)).weak());
            return;
        };
        let end = s.seq.end_tick();
        ui.horizontal(|ui| {
            ui.heading(&s.label);
            if s.edited() {
                ui.label(RichText::new("edited").color(Color32::from_rgb(255, 200, 90)));
            }
        });
        ui.horizontal(|ui| {
            let playing = st.playing;
            if ui.add_sized([70.0, 24.0], egui::Button::new(if playing { "Pause" } else { "Play" })).clicked() {
                self.engine.send(if playing { Cmd::Pause } else { Cmd::Play });
            }
            if ui.add_sized([60.0, 24.0], egui::Button::new("Stop")).clicked() {
                self.engine.send(Cmd::Stop);
            }
            ui.checkbox(&mut s.looping, "Loop").on_hover_text("Jump back at the end (the song's loop point, or the A-B markers); off: play to the end and stop");
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
                self.engine.send(Cmd::SeekTick(t));
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
            ui.label("Master");
            ui.add(egui::Slider::new(&mut s.master, 0.0..=2.0).fixed_decimals(2));
            ui.separator();
            if ui.button("Reset track").on_hover_text("Back to the track as on the disc: notes, mixer, instruments, chip, loop, tempo").clicked() {
                s.reset();
                self.engine.send(Cmd::Stop);
                self.engine.send(Cmd::Play);
                self.roll.selected = None;
            }
            ui.separator();
            let busy = format!("voices {:2}/48   {}", st.active_voices, if st.busy_ms > 0 { format!("re-sequence {} ms", st.busy_ms) } else { String::new() });
            ui.label(RichText::new(busy).monospace().weak());
        });
    }

    fn roll_tab(&mut self, ui: &mut egui::Ui) {
        let Some(s) = self.session.as_mut() else { return };
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
            ui.add(egui::Slider::new(&mut self.roll.px_per_tick, 0.02..=8.0).logarithmic(true).show_value(false));
            ui.add(egui::Slider::new(&mut self.roll.row_h, 3.0..=20.0).show_value(false)).on_hover_text("Row height");
            ui.separator();
            let sel = self.roll.selected.and_then(|(c, id)| self.notes.get(c)?.iter().find(|x| x.id == id).copied().map(|x| (c, x)));
            match sel {
                Some((c, note)) => {
                    let mut spec = NoteSpec { start: note.start, len: note.len, key: note.key, vel: note.vel };
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
                None => {
                    ui.label(RichText::new("click: add a note · drag: move · drag the end: length · right click: delete · velocity lane: drag").weak());
                }
            }
        });
        let typing = ui.ctx().egui_wants_keyboard_input();
        if !typing && ui.input(|i| i.key_pressed(egui::Key::Delete) || i.key_pressed(egui::Key::Backspace)) {
            delete = true;
        }
        if delete && let Some((c, id)) = self.roll.selected.take() {
            s.seq.set_note(c, id, None);
            s.seq_changed();
        }
        self.refresh_notes();
        let Some(s) = self.session.as_mut() else { return };
        if self.fit_pending {
            self.roll.fit(&self.notes, ui.available_width());
            self.fit_pending = false;
            // the toolbar above was drawn before: once more
            ui.ctx().request_repaint();
            if let Some(t) = self.scroll_to.take() {
                self.roll.scroll_tick = t.saturating_sub(PPQN * 4) as f32;
            }
        }
        let head = self.status.loaded.then_some(self.seek_drag.unwrap_or(self.status.tick));
        let follow = self.status.playing;
        let saved = self.roll.follow;
        self.roll.follow &= follow;
        let actions = self.roll.show(ui, s, &self.notes, head);
        self.roll.follow = saved;
        for a in actions {
            match a {
                roll::Action::Edited => s.seq_changed(),
                roll::Action::Seek(t) => self.engine.send(Cmd::SeekTick(t)),
                roll::Action::Loop(a, b) => {
                    s.set_loop(Some((a, b)));
                    s.looping = true;
                }
            }
        }
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
                        for h in ["", "ch", "voices", "program", "mute", "solo", "volume", "pan", "transpose", "instrument", ""] {
                            ui.label(RichText::new(h).strong());
                        }
                        ui.end_row();
                        for c in 0..s.channels.len() {
                            let (r, _) = ui.allocate_exact_size(egui::vec2(10.0, 16.0), egui::Sense::hover());
                            ui.painter().rect_filled(r, 2.0, channel_color(c));
                            ui.label(format!("{c}"));
                            let v = st.voices.get(c).copied().unwrap_or(0);
                            let (r, _) = ui.allocate_exact_size(egui::vec2(60.0, 12.0), egui::Sense::hover());
                            ui.painter().rect_filled(r, 2.0, Color32::from_gray(40));
                            let mut f = r;
                            f.set_width(r.width() * (v as f32 / 8.0).min(1.0));
                            ui.painter().rect_filled(f, 2.0, channel_color(c));
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
        ui.heading("Output");
        ui.horizontal(|ui| {
            ui.label("master volume");
            ui.add(egui::Slider::new(&mut s.master, 0.0..=2.0).fixed_decimals(2));
        });
        ui.label(RichText::new("After the chip: +9 dB and a -1 dBFS limiter, as the converter's music bank (docs/music.md).").weak());
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
        ui.label(RichText::new("Or drag the A and B markers in the piano roll's ruler. A studio loop restores the whole sequencer state of tick A (volumes and programs too); the song's own loop restores what the PS2 driver does.").weak());
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
}

fn chan_label(c: usize, notes: &[Vec<Note>]) -> String {
    let n = notes.get(c).map_or(0, Vec::len);
    format!("{c} ({n} notes)")
}

fn grid_name(g: u32) -> String {
    match g {
        192 => "1 bar".into(),
        48 => "1/4".into(),
        24 => "1/8".into(),
        12 => "1/16".into(),
        6 => "1/32".into(),
        16 => "1/8T".into(),
        8 => "1/16T".into(),
        1 => "tick".into(),
        g => format!("{g}"),
    }
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
                        }
                        self.sync();
                        if st.paused {
                            self.engine.send(Cmd::Pause);
                        }
                        if let Some(t) = st.seek {
                            self.engine.send(Cmd::SeekTick(t));
                            self.scroll_to = Some(t);
                        }
                    }
                    Some(None) => self.message = Some(format!("--open: no track {}", st.open.unwrap_or_default())),
                    None => {}
                }
            }
        }
        self.status = self.engine.status();
        if !ui.ctx().egui_wants_keyboard_input() && ui.input(|i| i.key_pressed(egui::Key::Space)) && self.session.is_some() {
            self.engine.send(if self.status.playing { Cmd::Pause } else { Cmd::Play });
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
                ui.selectable_value(&mut self.tab, Tab::Roll, RichText::new("Piano roll").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Mixer, RichText::new("Mixer & instruments").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Chip, RichText::new("Sound chip").size(15.0));
                ui.selectable_value(&mut self.tab, Tab::Song, RichText::new("Song: loop & tempo").size(15.0));
            });
            ui.separator();
            self.refresh_notes();
            match self.tab {
                Tab::Roll => self.roll_tab(ui),
                Tab::Mixer => self.mixer_tab(ui),
                Tab::Chip => self.chip_tab(ui),
                Tab::Song => self.song_tab(ui),
            }
        });
        self.sync();
        if self.status.playing {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
        }
    }
}
