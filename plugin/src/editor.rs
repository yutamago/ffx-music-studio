//! The plugin window (egui): the song, the chip's part and link, the
//! output, the 48 voices live, and where the instruments come from. Styled
//! after FFX's blue menu windows.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use nih_plug::prelude::*;
use nih_plug_egui::egui::{
    self, Align, Color32, CornerRadius, FontId, Layout, Margin, Mesh, Rect, RichText, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};
use nih_plug_egui::widgets::ParamSlider;
use nih_plug_egui::{EguiState, create_egui_editor};

use crate::engine::VOICES;
use crate::plugin::{Loader, Meters, Names, SoundParams, TempoParam};

const W: u32 = 640;
const H: u32 = 650;

const TEXT: Color32 = Color32::from_rgb(242, 245, 255);
const DIM: Color32 = Color32::from_rgb(150, 168, 214);
const GOLD: Color32 = Color32::from_rgb(240, 199, 94);
const GREEN: Color32 = Color32::from_rgb(111, 227, 155);
const RED: Color32 = Color32::from_rgb(255, 122, 122);
const AMBER: Color32 = Color32::from_rgb(255, 190, 90);
const BG_TOP: Color32 = Color32::from_rgb(18, 36, 86);
const BG_BOTTOM: Color32 = Color32::from_rgb(4, 9, 28);
const PANEL_TOP: Color32 = Color32::from_rgba_premultiplied(44, 82, 170, 200);
const PANEL_BOTTOM: Color32 = Color32::from_rgba_premultiplied(14, 30, 80, 210);
const BORDER: Color32 = Color32::from_rgb(176, 198, 246);

pub fn default_state() -> Arc<EguiState> {
    EguiState::from_size(W, H)
}

/// What the window reads and changes.
pub struct Shared {
    pub params: Arc<SoundParams>,
    pub names: Names,
    pub loader: Arc<Loader>,
    pub meters: Arc<Meters>,
}

struct State {
    shared: Shared,
    search: String,
    source_edit: String,
    /// Scroll the song list to the selected song once (on opening).
    scroll_to_selected: bool,
}

pub fn create(state: Arc<EguiState>, shared: Shared) -> Option<Box<dyn Editor>> {
    let source_edit = shared.params.source.lock().map(|s| s.clone()).unwrap_or_default();
    create_egui_editor(
        state,
        State { shared, search: String::new(), source_edit, scroll_to_selected: true },
        |ctx, _| style(ctx),
        |ctx, setter, st| {
            egui::CentralPanel::default().frame(egui::Frame::new()).show(ctx, |ui| {
                background(ui);
                egui::Frame::new().inner_margin(Margin::same(14)).show(ui, |ui| window(ui, setter, st));
            });
            ctx.request_repaint_after(Duration::from_millis(33));
        },
    )
}

fn style(ctx: &egui::Context) {
    ctx.style_mut(|s| {
        let v = &mut s.visuals;
        *v = egui::Visuals::dark();
        v.override_text_color = Some(TEXT);
        v.panel_fill = BG_BOTTOM;
        v.extreme_bg_color = Color32::from_rgb(8, 16, 42);
        v.selection.bg_fill = Color32::from_rgb(70, 110, 210);
        v.selection.stroke = Stroke::new(1.0, TEXT);
        v.hyperlink_color = GOLD;
        let w = &mut v.widgets;
        for (st, fill, stroke) in [
            (&mut w.inactive, Color32::from_rgb(28, 52, 118), Color32::from_rgb(92, 122, 196)),
            (&mut w.hovered, Color32::from_rgb(44, 74, 156), BORDER),
            (&mut w.active, Color32::from_rgb(62, 98, 196), TEXT),
            (&mut w.open, Color32::from_rgb(44, 74, 156), BORDER),
        ] {
            st.bg_fill = fill;
            st.weak_bg_fill = fill;
            st.bg_stroke = Stroke::new(1.0, stroke);
            st.fg_stroke = Stroke::new(1.0, TEXT);
            st.corner_radius = CornerRadius::same(5);
        }
        w.noninteractive.bg_stroke = Stroke::new(1.0, Color32::from_rgb(70, 96, 160));
        w.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 4.0);
        s.spacing.interact_size.y = 24.0;
    });
}

/// A vertical gradient over `r`.
fn gradient(ui: &Ui, r: Rect, top: Color32, bottom: Color32) {
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.left_bottom(), bottom);
    m.colored_vertex(r.right_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 2, 3);
    ui.painter().add(egui::Shape::mesh(m));
}

fn background(ui: &Ui) {
    gradient(ui, ui.max_rect(), BG_TOP, BG_BOTTOM);
}

/// An FFX menu window: a blue gradient, a light rim, a title.
fn panel<R>(ui: &mut Ui, title: &str, add: impl FnOnce(&mut Ui) -> R) -> R {
    let where_ = ui.painter().add(egui::Shape::Noop);
    let outer = ui.available_width();
    let out = egui::Frame::new().inner_margin(Margin::symmetric(12, 10)).show(ui, |ui| {
        ui.set_width(outer - 24.0);
        ui.label(RichText::new(title).size(12.5).color(GOLD).strong());
        ui.add_space(2.0);
        add(ui)
    });
    let r = out.response.rect;
    let mut m = Mesh::default();
    // the gradient as a rounded-ish quad, then the rim on top
    m.colored_vertex(r.left_top(), PANEL_TOP);
    m.colored_vertex(r.right_top(), PANEL_TOP);
    m.colored_vertex(r.left_bottom(), PANEL_BOTTOM);
    m.colored_vertex(r.right_bottom(), PANEL_BOTTOM);
    m.add_triangle(0, 1, 2);
    m.add_triangle(1, 2, 3);
    ui.painter().set(where_, egui::Shape::mesh(m));
    ui.painter().rect_stroke(r, CornerRadius::same(3), Stroke::new(1.5, BORDER), StrokeKind::Inside);
    ui.painter().rect_stroke(r.shrink(3.0), CornerRadius::same(2), Stroke::new(1.0, Color32::from_rgba_unmultiplied(176, 198, 246, 50)), StrokeKind::Inside);
    out.inner
}

fn set<P: Param>(setter: &ParamSetter, p: &P, v: P::Plain) {
    setter.begin_set_parameter(p);
    setter.set_parameter(p, v);
    setter.end_set_parameter(p);
}

/// Buttons side by side, one selected; returns a click on another.
fn segmented(ui: &mut Ui, labels: &[&str], selected: usize) -> Option<usize> {
    let mut out = None;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (i, l) in labels.iter().enumerate() {
            let on = i == selected;
            let b = egui::Button::new(RichText::new(*l).color(if on { Color32::from_rgb(20, 24, 40) } else { TEXT }))
                .fill(if on { GOLD } else { Color32::from_rgb(28, 52, 118) })
                .min_size(vec2(0.0, 24.0));
            if ui.add(b).clicked() && !on {
                out = Some(i);
            }
        }
    });
    out
}

fn window(ui: &mut Ui, setter: &ParamSetter, st: &mut State) {
    let p = st.shared.params.clone();
    // title
    ui.horizontal(|ui| {
        ui.label(RichText::new("FFX PS2 Sound").size(22.0).strong().color(TEXT));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new("PS2 sound driver · SPU2 · 48 voices").size(11.5).color(DIM));
        });
    });
    ui.add_space(4.0);
    status(ui, st);
    ui.add_space(8.0);

    let full = ui.available_width();
    let left = (full * 0.56).floor();
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(vec2(left, 300.0), Layout::top_down(Align::Min), |ui| {
            panel(ui, "SONG", |ui| songs(ui, setter, st));
        });
        ui.allocate_ui_with_layout(vec2(full - left - 8.0, 300.0), Layout::top_down(Align::Min), |ui| {
            panel(ui, "CHIP", |ui| {
                ui.label(RichText::new("Part").color(DIM));
                let part = p.part.value();
                if let Some(i) = segmented(ui, &["1", "2", "3"], (part - 1).clamp(0, 2) as usize) {
                    set(setter, &p.part, i as i32 + 1);
                }
                ui.label(RichText::new("Link").color(DIM));
                let link = p.link.value();
                egui::ComboBox::from_id_salt("link")
                    .width(ui.available_width() - 4.0)
                    .selected_text(if link == 0 { "off".to_string() } else { format!("group {link}") })
                    .show_ui(ui, |ui| {
                        for g in 0..=16 {
                            let text = if g == 0 { "off".to_string() } else { format!("group {g}") };
                            if ui.selectable_label(g == link, text).clicked() && g != link {
                                set(setter, &p.link, g);
                            }
                        }
                    });
                if link != 0 && part != 1 {
                    ui.label(RichText::new(format!("Sends its MIDI to Part 1 of group {link}; silent here.")).size(11.0).color(DIM));
                }
            });
            ui.add_space(8.0);
            panel(ui, "OUTPUT", |ui| {
                ui.label(RichText::new("Tempo").color(DIM));
                let t = if p.tempo.value() == TempoParam::Sequence { 0 } else { 1 };
                if let Some(i) = segmented(ui, &["PS2 timing", "Host"], t) {
                    set(setter, &p.tempo, if i == 0 { TempoParam::Sequence } else { TempoParam::Host });
                }
                let stage = p.stage.value();
                if let Some(i) = segmented(ui, &["PS2 stage on", "off"], if stage { 0 } else { 1 }) {
                    set(setter, &p.stage, i == 0);
                }
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Gain").color(DIM));
                    // the slider's value text takes about 40 px after it
                    ui.add(ParamSlider::for_param(&p.gain, setter).with_width((ui.available_width() - 52.0).max(40.0)));
                });
            });
        });
    });
    ui.add_space(8.0);
    panel(ui, "VOICES", |ui| voices(ui, &st.shared.meters));
    ui.add_space(8.0);
    source(ui, st);
}

fn status(ui: &mut Ui, st: &State) {
    let l = &st.shared.loader;
    let busy = l.busy.load(Ordering::Acquire);
    let ok = l.ok.load(Ordering::Acquire);
    let text = l.status.lock().map(|s| s.clone()).unwrap_or_default();
    egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(0, 0, 0, 70))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                if busy {
                    ui.add(egui::Spinner::new().size(14.0).color(AMBER));
                    ui.label(RichText::new("Loading the instruments…").color(AMBER));
                } else if text.is_empty() {
                    dot(ui, DIM);
                    ui.label(RichText::new("Waiting for the host to start the plugin").color(DIM));
                } else if ok {
                    dot(ui, GREEN);
                    let (song, from) = text.split_once(", ").unwrap_or((&text, ""));
                    ui.label(RichText::new(song).strong());
                    if !from.is_empty() {
                        ui.label(RichText::new(format!("· {from}")).size(11.0).color(DIM));
                    }
                } else {
                    dot(ui, RED);
                    ui.add(egui::Label::new(RichText::new(&text).size(11.5).color(RED)).wrap());
                }
            });
        });
}

fn dot(ui: &mut Ui, c: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(12.0, 12.0), Sense::hover());
    ui.painter().circle_filled(r.center(), 5.0, c);
    ui.painter().circle_stroke(r.center(), 6.5, Stroke::new(1.0, c.gamma_multiply(0.4)));
}

fn songs(ui: &mut Ui, setter: &ParamSetter, st: &mut State) {
    let p = st.shared.params.clone();
    ui.add(egui::TextEdit::singleline(&mut st.search).hint_text(RichText::new("Search songs (name or number)").color(DIM)).desired_width(f32::INFINITY));
    let names: Vec<(i32, String)> = st.shared.names.read().map(|m| m.iter().map(|(k, v)| (*k, v.clone())).collect()).unwrap_or_default();
    let q = st.search.trim().to_lowercase();
    let cur = p.track.value();
    let scroll = std::mem::take(&mut st.scroll_to_selected);
    egui::ScrollArea::vertical().max_height(196.0).auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        if names.is_empty() {
            ui.label(RichText::new("The song list appears once the instruments are loaded.").color(DIM));
        }
        if q.is_empty() || "bank".contains(&q) {
            if row(ui, "---", "A bank by number (no song)", cur == 0, scroll && cur == 0) {
                set(setter, &p.track, 0);
            }
        }
        for (id, label) in &names {
            let title = label.split_once(' ').map_or("(no name)", |x| x.1);
            if !q.is_empty() && !label.to_lowercase().contains(&q) && !format!("{id}").starts_with(&q) {
                continue;
            }
            if row(ui, &format!("{id:03}"), title, *id == cur, scroll && *id == cur) {
                set(setter, &p.track, *id);
            }
        }
    });
    if cur == 0 {
        ui.add_space(4.0);
        let n = st.shared.loader.banks.load(Ordering::Acquire);
        ui.horizontal(|ui| {
            ui.label(RichText::new("Bank").color(DIM));
            ui.add(ParamSlider::for_param(&p.bank, setter).with_width((ui.available_width() - 52.0).max(40.0)));
        });
        if n > 0 {
            ui.label(RichText::new(format!("0 to {} (the \"SF2/MIDI bank\" column of HOW-TO-OPEN.md)", n - 1)).size(11.0).color(DIM));
        }
    }
}

/// One song in the list; returns a click.
fn row(ui: &mut Ui, num: &str, title: &str, selected: bool, scroll: bool) -> bool {
    let w = ui.available_width();
    let (r, resp) = ui.allocate_exact_size(vec2(w, 22.0), Sense::click());
    let pt = ui.painter();
    if selected {
        pt.rect_filled(r, CornerRadius::same(4), Color32::from_rgba_unmultiplied(240, 199, 94, 70));
        pt.rect_stroke(r, CornerRadius::same(4), Stroke::new(1.0, GOLD), StrokeKind::Inside);
        // the FFX hand cursor's place: a small pointer
        let c = pos2(r.left() + 7.0, r.center().y);
        pt.add(egui::Shape::convex_polygon(vec![c + vec2(-3.0, -4.0), c + vec2(3.0, 0.0), c + vec2(-3.0, 4.0)], GOLD, Stroke::NONE));
    } else if resp.hovered() {
        pt.rect_filled(r, CornerRadius::same(4), Color32::from_rgba_unmultiplied(120, 160, 255, 30));
    }
    pt.text(pos2(r.left() + 16.0, r.center().y), egui::Align2::LEFT_CENTER, num, FontId::monospace(12.0), if selected { GOLD } else { DIM });
    pt.text(pos2(r.left() + 52.0, r.center().y), egui::Align2::LEFT_CENTER, title, FontId::proportional(13.0), if selected { TEXT } else { TEXT.gamma_multiply(0.92) });
    if scroll {
        resp.scroll_to_me(Some(Align::Center));
    }
    resp.clicked()
}

/// The 48 voices (three rows of 16, as the MIDI ports), coloured by the
/// MIDI channel playing them, and the output level.
fn voices(ui: &mut Ui, m: &Meters) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 62.0), Sense::hover());
    let pt = ui.painter();
    let meter_w = 26.0;
    let grid = Rect::from_min_max(r.min, pos2(r.max.x - meter_w - 10.0, r.max.y));
    let cw = grid.width() / 16.0;
    let ch = grid.height() / 3.0;
    let mut sounding = 0;
    for v in 0..VOICES {
        let lv = f32::from(m.level[v].load(Ordering::Relaxed)) / 255.0;
        let tr = m.track[v].load(Ordering::Relaxed);
        let cell = Rect::from_min_size(pos2(grid.left() + (v % 16) as f32 * cw, grid.top() + (v / 16) as f32 * ch), vec2(cw, ch)).shrink(2.0);
        pt.rect_filled(cell, CornerRadius::same(3), Color32::from_rgba_unmultiplied(0, 0, 0, 90));
        if tr != 255 && lv > 0.0 {
            sounding += 1;
            let c = channel_colour(tr);
            let a = 0.25 + 0.75 * lv;
            pt.rect_filled(cell, CornerRadius::same(3), c.gamma_multiply(a));
            let bar = Rect::from_min_max(pos2(cell.left(), cell.bottom() - cell.height() * lv), cell.max);
            pt.rect_filled(bar, CornerRadius::same(3), c);
        }
        pt.rect_stroke(cell, CornerRadius::same(3), Stroke::new(1.0, Color32::from_rgba_unmultiplied(176, 198, 246, 40)), StrokeKind::Inside);
    }
    // the output level, left and right
    for i in 0..2 {
        let pk = f32::from_bits(m.peak[i].load(Ordering::Relaxed)).clamp(0.0, 1.0);
        let x = r.max.x - meter_w + i as f32 * (meter_w / 2.0);
        let bar = Rect::from_min_max(pos2(x, r.top() + 2.0), pos2(x + meter_w / 2.0 - 3.0, r.bottom() - 2.0));
        pt.rect_filled(bar, CornerRadius::same(2), Color32::from_rgba_unmultiplied(0, 0, 0, 110));
        // dB scale: -48..0 dBFS
        let db = if pk > 0.0 { 20.0 * pk.log10() } else { -96.0 };
        let f = ((db + 48.0) / 48.0).clamp(0.0, 1.0);
        let fill = Rect::from_min_max(pos2(bar.left(), bar.bottom() - bar.height() * f), bar.max);
        let c = if db > -1.5 { RED } else if db > -9.0 { GOLD } else { GREEN };
        pt.rect_filled(fill, CornerRadius::same(2), c);
    }
    ui.label(RichText::new(format!("{sounding} of 48 voices sounding · rows: MIDI port 1-3 · colour: channel")).size(11.0).color(DIM));
}

fn channel_colour(track: u8) -> Color32 {
    // 16 hues, one per MIDI channel (the same on every port)
    let h = f32::from(track % 16) / 16.0;
    let c = egui::ecolor::Hsva::new(h, 0.62, 1.0, 1.0);
    Color32::from(c)
}

fn source(ui: &mut Ui, st: &mut State) {
    let p = st.shared.params.clone();
    egui::Frame::new()
        .fill(Color32::from_rgba_unmultiplied(0, 0, 0, 60))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Instruments from").color(DIM));
                let apply_w = 132.0;
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut st.source_edit)
                        .hint_text(RichText::new("found by itself (export folder, disc .toml)").color(DIM))
                        .desired_width(ui.available_width() - apply_w),
                );
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.button("Use").clicked() || enter {
                    if let Ok(mut s) = p.source.lock() {
                        *s = st.source_edit.trim().trim_matches('"').to_string();
                    }
                }
                if ui.button("Auto").on_hover_text("Find the export folder or a disc .toml by itself").clicked() {
                    st.source_edit.clear();
                    if let Ok(mut s) = p.source.lock() {
                        s.clear();
                    }
                }
            });
            ui.label(RichText::new("An export folder (with banks\\) or a demaster.toml naming your PS2 disc.").size(10.5).color(DIM));
            let log = crate::source::log_path().map(|p| p.display().to_string()).unwrap_or_default();
            ui.label(RichText::new(format!("Log: {log}")).size(10.5).color(DIM));
        });
}
