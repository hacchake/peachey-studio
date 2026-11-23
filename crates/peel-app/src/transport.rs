//! Top bar: project, View, history, transport, time, master meter and
//! Export. Also the pattern picker the dock header uses.

use peel_document::{Id, PALETTE, Pattern, Time};
use peel_media::AudioFormat;
use peel_session::PlayMode;
use peel_ui::Id as W;
use peel_ui::{Align, MenuBar, MenuItem, Rect, Ui, WindowControl, icons, vec2};

use crate::studio::Studio;
use crate::util::{col, format_position};

/// What the top bar shows at a given width; later fields give way first
/// to earlier ones as the window narrows.
#[derive(Clone, Copy)]
struct Fit {
    load_text: bool,
    wind: bool,
    meter: bool,
    export_label: bool,
    clock: bool,
    title_w: f32,
    /// View as an icon with its caret, no label.
    view_icon: bool,
}

impl Fit {
    fn level(n: usize, title_w: f32) -> Fit {
        Fit {
            load_text: n < 1,
            wind: n < 2,
            meter: n < 3,
            export_label: n < 4,
            clock: n < 5,
            title_w: match n {
                0..=4 => title_w,
                5 => title_w.min(96.0),
                _ => 0.0,
            },
            view_icon: n >= 7,
        }
    }

    fn view_w(&self, full: f32) -> f32 {
        if self.view_icon { 44.0 } else { full }
    }

    fn left(&self, view_w: f32) -> f32 {
        let view_w = self.view_w(view_w);
        let title = if self.title_w > 0.0 {
            self.title_w + 12.0
        } else {
            0.0
        };
        28.0 + 8.0 + title + view_w + 6.0 + 60.0
    }

    fn playback(&self) -> f32 {
        4.0 + 120.0
            + 13.0
            + 30.0 * 2.0
            + 34.0 * 3.0
            + 13.0
            + 30.0
            + 4.0
            + if self.wind { 60.0 } else { 0.0 }
    }

    fn time(&self) -> f32 {
        4.0 + self.pos_w() + 13.0 + 88.0 + 13.0 + 40.0 + 13.0 + 30.0 + 4.0
    }

    fn pos_w(&self) -> f32 {
        if self.clock { 124.0 } else { 84.0 }
    }

    fn right(&self, meter_on: bool) -> f32 {
        let export = if self.export_label { 92.0 } else { 36.0 };
        let meter = if meter_on && self.meter {
            12.0 + 96.0 + 10.0 + if self.load_text { 46.0 } else { 18.0 }
        } else {
            0.0
        };
        export + 8.0 + 60.0 + meter
    }
}

/// Top bar: project and View on the left, transport and time centered in
/// two wells, output (meter, open, save, Export) on the right.
pub fn draw(ui: &mut Ui, r: Rect, win: &WindowControl, studio: &mut Studio) {
    let t = ui.theme.clone();
    // The whole bar moves the window; controls registered later take priority.
    let bar = ui.interact(W::new("titlebar"), r);
    if bar.pressed && !bar.double_clicked {
        win.drag_window();
    }
    if bar.double_clicked {
        win.toggle_maximized();
    }
    // The bar is a toolbar surface; a dark seam separates it from the panes.
    ui.scene.rect(r, 0.0, t.panel);
    ui.scene.hline(r.x, r.right(), r.bottom() - 0.5, t.bg);

    let mut row = r.shrink2(8.0, 6.0);
    row.cut_left(win.titlebar_inset());
    let title = studio.session.project().meta.title.clone();
    let title_w = ui.measure(&title, t.label).x.min(180.0) + 2.0;
    // Without a platform menu bar the app menus sit around View: File and
    // Edit before it, Transport and Help after. A tight bar keeps File
    // only (undo, transport and help have their own controls).
    let strip = (!win.has_native_menu() || studio.menu_strip).then(|| {
        let bar = crate::menus::bar(studio, false);
        let (before, after): (Vec<_>, Vec<_>) = bar
            .menus
            .into_iter()
            .filter(|m| m.title != "View")
            .partition(|m| m.title == "File" || m.title == "Edit");
        let file = before
            .iter()
            .filter(|m| m.title == "File")
            .cloned()
            .collect();
        let full = (MenuBar { menus: before }, MenuBar { menus: after });
        let full_w = ui.menu_bar_width(&full.0) + ui.menu_bar_width(&full.1);
        let compact = (MenuBar { menus: file }, MenuBar::default());
        let compact_w = ui.menu_bar_width(&compact.0);
        (full, full_w, compact, compact_w)
    });
    let view_w = ui.quiet_button_width(true, "View", true);
    let meter_on = studio.prefs.show_meter;
    let fits = |f: &Fit, strip_w: f32| {
        strip_w + f.left(view_w) + f.playback() + 8.0 + f.time() + f.right(meter_on) + 32.0 <= row.w
    };
    // The logo gives way last, and only to the File menu.
    let mut logo_shown = true;
    let (fit, strip) = match strip {
        None => (
            (0..=7)
                .map(|n| Fit::level(n, title_w))
                .find(|f| fits(f, 0.0))
                .unwrap_or_else(|| Fit::level(7, title_w)),
            None,
        ),
        Some((full, full_w, compact, compact_w)) => {
            match (0..=4)
                .map(|n| Fit::level(n, title_w))
                .find(|f| fits(f, full_w))
            {
                Some(f) => (f, Some(full)),
                None => (
                    (0..=7)
                        .map(|n| Fit::level(n, title_w))
                        .find(|f| fits(f, compact_w))
                        .unwrap_or_else(|| {
                            logo_shown = false;
                            Fit::level(7, title_w)
                        }),
                    Some(compact),
                ),
            }
        }
    };

    // ---- Left: identity, View, history.
    if logo_shown {
        logo(ui, row.cut_left(28.0));
        row.cut_left(8.0);
    }
    if fit.title_w > 0.0 {
        let title_r = row.cut_left(fit.title_w);
        let fitted = ui.fit_text(&title, t.label, title_r.w);
        ui.text(title_r, &fitted, t.label, t.text, Align::Start);
        ui.describe_label(W::new("project-title"), title_r, &title);
        row.cut_left(12.0);
    }
    let mut chosen = None;
    if let Some((before, _)) = &strip {
        let w = ui.menu_bar_width(before);
        chosen = ui.menu_bar(crate::menus::STRIP, row.cut_left(w), before);
    }
    crate::layout::view_button(ui, row.cut_left(fit.view_w(view_w)), studio, fit.view_icon);
    if let Some((_, after)) = &strip
        && !after.menus.is_empty()
    {
        let w = ui.menu_bar_width(after);
        chosen = chosen.or(ui.menu_bar(crate::menus::STRIP.with("after"), row.cut_left(w), after));
    }
    if let Some(id) = chosen {
        ui.queue_menu_command(id);
    }
    row.cut_left(6.0);
    let undo = ui.icon_button(W::new("undo"), row.cut_left(30.0), icons::UNDO, false, None);
    let undo_label = studio
        .session
        .doc
        .undo_label()
        .map(|l| format!("Undo {l}"))
        .unwrap_or_else(|| "Nothing to undo".into());
    ui.tooltip(&undo, &format!("{undo_label}  ·  right-click for history"));
    if undo.clicked {
        studio.session.doc.undo();
    }
    if undo.secondary_clicked {
        ui.open_popup(W::new("history"), undo.rect);
    }
    let redo = ui.icon_button(W::new("redo"), row.cut_left(30.0), icons::REDO, false, None);
    let redo_label = studio
        .session
        .doc
        .redo_label()
        .map(|l| format!("Redo {l}"))
        .unwrap_or_else(|| "Nothing to redo".into());
    ui.tooltip(&redo, &redo_label);
    if redo.clicked {
        studio.session.doc.redo();
    }
    history_panel(ui, undo.rect, studio);

    // ---- Right: Export (the bar's one primary action), save, open, meter.
    let mut right = row;
    let export_r = right.cut_right(if fit.export_label { 92.0 } else { 36.0 });
    let export = ui.interact_button(W::new("export"), export_r, "Export");
    {
        let er = export.rect;
        let hot = ui.animate_bool(W::new("export-hot"), export.hovered);
        ui.scene.rect(er, t.radius_m, t.accent.lighten(hot * 0.08));
        if fit.export_label {
            let mut inner = er.shrink2(12.0, 0.0);
            ui.icon(inner.cut_left(16.0), icons::EXPORT, 15.0, t.text_on_accent);
            ui.text(inner, "Export", t.label, t.text_on_accent, Align::Center);
        } else {
            ui.icon(er, icons::EXPORT, 16.0, t.text_on_accent);
        }
        if export.hovered {
            ui.set_cursor(peel_ui::CursorIcon::Pointer);
        }
        ui.focus_ring(export.id, er, t.radius_m);
    }
    ui.tooltip(
        &export,
        "Export the arrangement as audio or MIDI (Cmd/Ctrl+E)",
    );
    if export.clicked {
        studio.export();
    }
    export_panel(ui, export.rect, studio);
    right.cut_right(8.0);
    let save = ui.icon_button(
        W::new("save"),
        right.cut_right(30.0),
        icons::SAVE,
        false,
        None,
    );
    ui.tooltip(&save, "Save (Cmd/Ctrl+S)");
    if save.clicked {
        studio.save();
    }
    let open = ui.icon_button(
        W::new("open"),
        right.cut_right(30.0),
        icons::FOLDER_OPEN,
        false,
        None,
    );
    ui.tooltip(&open, "Open project or MIDI file (Cmd/Ctrl+O)");
    if open.clicked {
        studio.open();
    }
    if meter_on && fit.meter {
        right.cut_right(12.0);
        let meter_r = right.cut_right(96.0).shrink2(0.0, 9.0);
        let master = studio.session.project().mixer.tracks[0].id;
        let peaks = crate::util::track_peaks(ui, &studio.session, master);
        horizontal_meter(ui, W::new("master-meter"), meter_r, peaks);
        right.cut_right(10.0);
        let load = studio.session.cpu_load();
        let load_r = right.cut_right(if fit.load_text { 46.0 } else { 18.0 });
        let lc = if load > 0.85 {
            t.record
        } else if load > 0.6 {
            t.warn
        } else {
            t.text_faint
        };
        let lresp = ui.interact(W::new("load"), load_r);
        ui.tooltip(
            &lresp,
            &format!("Audio processing load {:.0}%", load * 100.0),
        );
        let mut lr = load_r;
        ui.icon(lr.cut_left(16.0), icons::CPU, 14.0, lc);
        if fit.load_text {
            ui.text(
                lr,
                &format!("{:.0}%", load * 100.0),
                t.readout_small,
                lc,
                Align::End,
            );
        }
    }

    // ---- Center: playback well and time well, centered on the window
    // when there is room, otherwise between the two sides.
    let group_w = fit.playback() + 8.0 + fit.time();
    let free = Rect {
        x: row.x + 16.0,
        w: (right.right() - row.x - 32.0).max(0.0),
        ..row
    };
    let x = (r.center().x - group_w * 0.5)
        .min(free.right() - group_w)
        .max(free.x);
    let mut center = Rect {
        x,
        w: group_w,
        ..row
    };
    let playback_r = center.cut_left(fit.playback());
    center.cut_left(8.0);
    let time_r = center;
    playback(ui, playback_r, studio, fit);
    time(ui, time_r, studio, fit);
}

/// Pattern/Song and the transport buttons, in one well.
fn playback(ui: &mut Ui, r: Rect, studio: &mut Studio, fit: Fit) {
    let t = ui.theme.clone();
    ui.well(r);
    let mut row = r.shrink2(4.0, 2.0);
    // Pattern / Song.
    let mut mode_idx = if studio.session.mode() == PlayMode::Song {
        1
    } else {
        0
    };
    if ui.segmented(
        W::new("mode"),
        row.cut_left(120.0),
        &["Pattern", "Song"],
        &mut mode_idx,
    ) {
        let mode = if mode_idx == 1 {
            PlayMode::Song
        } else {
            PlayMode::Pattern(studio.current_pattern())
        };
        studio.session.set_mode(mode);
    }
    ui.divider_v(row.cut_left(13.0));

    let playing = studio.session.is_playing();
    let bar = studio
        .session
        .project()
        .time_signature
        .bar_length()
        .as_f64();
    let start = ui.icon_button_fill(
        W::new("to-start"),
        row.cut_left(30.0),
        icons::SKIP_BACK,
        false,
        t.text,
        13.0,
    );
    ui.tooltip(&start, "Go to start (Home)");
    if start.clicked {
        studio.session.seek(0.0);
    }
    if fit.wind {
        let rew = ui.icon_button_fill(
            W::new("rewind"),
            row.cut_left(30.0),
            icons::REWIND,
            false,
            t.text,
            13.0,
        );
        ui.tooltip(&rew, "Back one bar (Page Up)");
        if rew.clicked {
            studio.session.nudge(-bar);
        }
    }
    let play = ui.key_button(
        W::new("play"),
        row.cut_left(34.0),
        if playing { icons::PAUSE } else { icons::PLAY },
        playing,
        t.play,
        16.0,
    );
    ui.tooltip(&play, "Play (Space)");
    if play.clicked {
        studio.session.toggle_play();
    }
    let stop = ui.key_button(
        W::new("stop"),
        row.cut_left(34.0),
        icons::STOP,
        false,
        t.text,
        14.0,
    );
    ui.tooltip(&stop, "Stop and return");
    if stop.clicked {
        if playing {
            if studio.session.performing() {
                studio.session.stop_performance();
            } else {
                studio.session.stop();
            }
        } else {
            studio.session.seek(0.0);
        }
    }
    let armed = studio.recorder.armed;
    let rec = ui.key_button(
        W::new("rec"),
        row.cut_left(34.0),
        icons::RECORD,
        armed,
        t.record,
        14.0,
    );
    let song = studio.session.mode() == peel_session::PlayMode::Song;
    ui.tooltip(
        &rec,
        match (armed, song) {
            (true, true) => "Recording audio onto armed lanes",
            (false, true) => "Record audio onto armed lanes",
            (true, false) => "Recording notes into the pattern",
            (false, false) => "Record notes into the pattern",
        },
    );
    if rec.clicked {
        studio.recorder.armed = !armed;
    }
    if fit.wind {
        let ff = ui.icon_button_fill(
            W::new("fast-forward"),
            row.cut_left(30.0),
            icons::FAST_FORWARD,
            false,
            t.text,
            13.0,
        );
        ui.tooltip(&ff, "Forward one bar (Page Down)");
        if ff.clicked {
            studio.session.nudge(bar);
        }
    }
    let end = ui.icon_button_fill(
        W::new("to-end"),
        row.cut_left(30.0),
        icons::SKIP_FORWARD,
        false,
        t.text,
        13.0,
    );
    ui.tooltip(&end, "Go to end (End)");
    if end.clicked {
        let to = studio.session.play_end();
        studio.session.seek(to);
    }
    ui.divider_v(row.cut_left(13.0));
    let looping = !song || studio.session.looping();
    let lp = ui.icon_button_fill(
        W::new("loop"),
        row.cut_left(30.0),
        icons::REPEAT,
        looping,
        t.accent,
        14.0,
    );
    ui.tooltip(
        &lp,
        match (song, looping) {
            (false, _) => "Patterns always loop",
            (true, true) => "Looping the song",
            (true, false) => "Loop the song",
        },
    );
    if lp.clicked && song {
        studio.session.set_looping(!looping);
    }
}

/// Position, tempo, time signature and metronome, in one well.
fn time(ui: &mut Ui, r: Rect, studio: &mut Studio, fit: Fit) {
    let t = ui.theme.clone();
    // A display window set into the bar: lit figures on black.
    ui.scene
        .rect_stroke(r, t.radius_m, t.bg, 1.0, t.line.alpha(0.9));
    let mut row = r.shrink2(4.0, 2.0);

    // Position readout.
    let pos_r = row.cut_left(fit.pos_w());
    let beat = studio.session.position();
    let sig = studio.session.project().time_signature;
    let pos = format_position(beat, sig);
    let mut pr = pos_r.shrink2(8.0, 0.0);
    let secs = studio
        .session
        .project()
        .tempo
        .first()
        .map_or(0.0, |s| beat * 60.0 / s.bpm);
    let time = format!("{}:{:04.1}", (secs / 60.0) as i64, secs % 60.0);
    ui.text(
        pr.cut_left(68.0),
        &pos,
        t.readout.size(18.0),
        t.display,
        Align::Start,
    );
    if fit.clock {
        ui.text(pr, &time, t.readout_small, t.text_faint, Align::End);
    }
    ui.describe_node(
        W::new("position"),
        peel_ui::Semantics::new(peel_ui::Role::Label)
            .label("Position")
            .value(format!("{pos}  {time}"))
            .rect(pos_r),
    );
    ui.divider_v(row.cut_left(13.0));

    // Tempo. With tempo changes, the tempo at the playhead: live while
    // playing (ramps included), the change in force when stopped. Edits
    // change that tempo change.
    let bpm_r = row.cut_left(88.0);
    let segs = studio.session.project().tempo.clone();
    let pos = studio.session.position().max(0.0);
    let active = segs
        .iter()
        .rposition(|x| x.start.as_f64() <= pos + 1e-9)
        .unwrap_or(0);
    let mut bpm = match segs.len() {
        0 | 1 => studio.session.project().bpm(),
        _ if studio.session.is_playing() => peel_time::TempoMap::from_segments(&segs).bpm_at(pos),
        _ => segs[active].bpm,
    };
    let mut br = bpm_r;
    let label = br.cut_right(30.0);
    let resp = ui.drag_number(
        W::new("bpm"),
        br,
        &mut bpm,
        20.0,
        400.0,
        0.2,
        t.readout.size(18.0),
        t.display,
        |v| format!("{v:.1}"),
    );
    // Tap tempo: click the BPM label in time with the music.
    let tap = ui.interact_button(W::new("tap-tempo"), label, "Tap tempo");
    let tapped = ui.animate(W::new("tap-flash"), 0.0, 8.0);
    ui.text(
        label,
        "BPM",
        t.caption,
        t.text_faint.mix(t.accent, tapped),
        Align::Start,
    );
    ui.tooltip(&tap, "Tap to set the tempo");
    if tap.hovered {
        ui.set_cursor(peel_ui::CursorIcon::Pointer);
    }
    if tap.pressed || (tap.clicked && !tap.hovered) {
        let now = ui.time();
        ui.animate(W::new("tap-flash"), 1.0, 1000.0);
        let taps = ui.state::<Vec<f64>>(W::new("tap-times"));
        if taps.last().is_some_and(|last| now - last > 2.0) {
            taps.clear();
        }
        taps.push(now);
        if taps.len() > 8 {
            taps.remove(0);
        }
        if taps.len() >= 3 {
            let span = taps[taps.len() - 1] - taps[0];
            let bpm = 60.0 * (taps.len() - 1) as f64 / span.max(0.01);
            if (30.0..=300.0).contains(&bpm) {
                let bpm = (bpm * 10.0).round() / 10.0;
                studio
                    .session
                    .doc
                    .edit_gesture(W::new("tap-gesture").0, "Tap tempo", |p| p.set_bpm(bpm));
            }
        }
    }
    if resp.changed {
        studio
            .session
            .doc
            .edit_gesture(W::new("bpm-gesture").0, "Change tempo", |p| {
                let v = ((bpm * 10.0).round() / 10.0).clamp(10.0, 999.0);
                if p.tempo.len() > 1 {
                    crate::playlist::tidy_tempo(&mut p.tempo);
                    if let Some(x) = p.tempo.get_mut(active) {
                        x.bpm = v;
                    }
                    crate::playlist::tidy_tempo(&mut p.tempo);
                } else {
                    p.set_bpm(v);
                }
            });
    }
    if resp.released {
        studio.session.doc.end_gesture();
    }
    ui.tooltip(&resp, "Drag or double-click to type");
    ui.divider_v(row.cut_left(13.0));

    // Time signature.
    let sig_r = row.cut_left(40.0);
    let sig_txt = format!("{}/{}", sig.numerator, sig.denominator);
    let sresp = ui.interact_button(W::new("sig"), sig_r, "Time signature");
    ui.describe_node(
        sresp.id,
        peel_ui::Semantics::patch().value(sig_txt.as_str()),
    );
    let hot = ui.animate_bool(W::new("sig"), sresp.hovered);
    if hot > 0.0 {
        ui.scene.rect(sig_r, t.radius_m, t.control.alpha(hot));
    }
    ui.text(
        sig_r,
        &sig_txt,
        t.readout_small.size(14.0),
        t.text_dim.mix(t.text, hot),
        Align::Center,
    );
    ui.tooltip(&sresp, "Time signature");
    if sresp.hovered {
        ui.set_cursor(peel_ui::CursorIcon::Pointer);
    }
    ui.focus_ring(sresp.id, sig_r, t.radius_m);
    if sresp.clicked {
        ui.open_popup(W::new("sig-menu"), sig_r);
    }
    let sigs = [(4u8, 4u8), (3, 4), (6, 8), (5, 4), (7, 8), (2, 4)];
    let items: Vec<MenuItem> = sigs
        .iter()
        .map(|(n, d)| {
            MenuItem::new(format!("{n}/{d}")).checked(sig.numerator == *n && sig.denominator == *d)
        })
        .collect();
    if let Some(i) = ui.menu(
        W::new("sig-menu"),
        vec2(sig_r.x, sig_r.bottom() + 4.0),
        &items,
    ) {
        let (n, d) = sigs[i];
        studio.session.doc.edit("Change time signature", |p| {
            p.time_signature = peel_document::TimeSignature {
                numerator: n,
                denominator: d,
            };
        });
    }
    ui.divider_v(row.cut_left(13.0));

    let metro_on = studio.session.metronome();
    let m = ui.icon_button(
        W::new("metronome"),
        row.cut_left(30.0),
        icons::METRONOME,
        metro_on,
        None,
    );
    ui.tooltip(&m, "Metronome");
    if m.clicked {
        studio.session.set_metronome(!metro_on);
    }
}

fn logo(ui: &mut Ui, r: Rect) {
    crate::util::logo(ui, r.center(), 12.0);
}

fn horizontal_meter(ui: &mut Ui, id: W, r: Rect, peaks: (f32, f32)) {
    // Reuse the vertical meter ballistics by drawing two thin horizontal bars.
    let t = ui.theme.clone();
    let st = ui.state::<[f32; 2]>(id);
    let dt = 1.0 / 60.0;
    for (i, p) in [peaks.0, peaks.1].into_iter().enumerate() {
        let v = peel_ui::meter_pos(p);
        st[i] = if v >= st[i] {
            v
        } else {
            (st[i] - dt * 0.35).max(v)
        };
    }
    let levels = *st;
    if levels[0] > 0.0 || levels[1] > 0.0 {
        ui.request_repaint();
    }
    let rows = r.rows(2, 3.0);
    for (i, row) in rows.iter().enumerate() {
        ui.scene.rect(*row, 0.0, t.bg);
        let w = row.w * levels[i];
        if w > 0.5 {
            let fill = row.with_w(w);
            let amber = peel_ui::meter_pos(0.5);
            let red = peel_ui::meter_pos(1.0);
            ui.scene.push_clip(fill);
            ui.scene.rect(
                Rect {
                    w: row.w * amber,
                    ..*row
                },
                0.0,
                t.play,
            );
            ui.scene.rect(
                Rect {
                    x: row.x + row.w * amber,
                    w: row.w * (red - amber),
                    ..*row
                },
                0.0,
                t.accent,
            );
            ui.scene.rect(
                Rect {
                    x: row.x + row.w * red,
                    w: row.w * (1.0 - red),
                    ..*row
                },
                0.0,
                t.record,
            );
            ui.scene.pop_clip();
        }
    }
    // 0 dB tick.
    let x = r.x + r.w * peel_ui::meter_pos(1.0);
    ui.scene
        .vline(x, r.y - 2.0, r.bottom() + 2.0, t.text_faint.alpha(0.6));
}

/// The pattern being edited, with its menu and a new-pattern button; sits
/// in the dock header beside the Channels and Piano roll editors.
pub fn pattern_picker(ui: &mut Ui, row: &mut Rect, studio: &mut Studio, width: f32) {
    let t = ui.theme.clone();
    let pid = studio.current_pattern();
    let Some(pat) = studio.session.project().pattern(pid).cloned() else {
        return;
    };
    let r = row.cut_left(width);
    let id = W::new("pattern-picker");
    let resp = ui.interact_button(id, r, "Pattern");
    ui.describe_node(id, peel_ui::Semantics::patch().value(pat.name.as_str()));
    let hot = ui.animate_bool(id, resp.hovered);
    ui.scene
        .rect(r, t.radius_m, t.control.mix(t.control_hover, hot));
    ui.focus_ring(id, r, t.radius_m);
    // Narrow docks show only the swatch and caret; the name stays in the
    // tooltip and the accessible value.
    let compact = width < 80.0;
    let mut inner = r.shrink2(if compact { 8.0 } else { 10.0 }, 0.0);
    let sw = inner.cut_left(12.0);
    ui.scene.rect(
        Rect::from_center(sw.center(), 10.0, 10.0),
        1.0,
        col(pat.color),
    );
    let caret = inner.cut_right(if compact { 12.0 } else { 16.0 });
    ui.icon(caret, icons::CARET_DOWN, 13.0, t.text_dim);
    if compact {
        ui.tooltip(&resp, &format!("Pattern being edited: {}", pat.name));
    } else {
        inner.cut_left(8.0);
        let name = ui.fit_text(&pat.name, t.label, inner.w);
        ui.text(inner, &name, t.label, t.text, Align::Start);
        ui.tooltip(&resp, "Pattern being edited");
    }
    if resp.clicked {
        ui.open_popup(W::new("pattern-menu"), r);
    }
    if resp.hovered {
        ui.set_cursor(peel_ui::CursorIcon::Pointer);
    }
    let patterns: Vec<(Id, String, peel_document::Color)> = studio
        .session
        .project()
        .patterns
        .iter()
        .map(|p| (p.id, p.name.clone(), p.color))
        .collect();
    let mut items: Vec<MenuItem> = patterns
        .iter()
        .map(|(id, name, c)| {
            MenuItem::new(name.clone())
                .swatch(col(*c))
                .checked(*id == pid)
        })
        .collect();
    items.push(MenuItem::Separator);
    items.push(MenuItem::new("New pattern"));
    items.push(MenuItem::new("Duplicate pattern"));
    items.push(MenuItem::new("Rename pattern"));
    items.push(MenuItem::new("Delete pattern").enabled(patterns.len() > 1));
    if let Some(i) = ui.menu(W::new("pattern-menu"), vec2(r.x, r.bottom() + 4.0), &items) {
        let n = patterns.len();
        if i < n {
            studio.session.select_pattern(patterns[i].0);
        } else if i == n + 1 {
            let id = new_pattern(studio, None);
            studio.session.select_pattern(id);
        } else if i == n + 2 {
            let id = new_pattern(studio, Some(&pat));
            studio.session.select_pattern(id);
        } else if i == n + 3 {
            crate::rename::open(ui, W::new("rename-pattern"), r, &pat.name);
        } else if i == n + 4 {
            delete_pattern(studio, pid);
        }
    }
    if let Some(name) = crate::rename::show(ui, W::new("rename-pattern")) {
        studio.session.doc.edit("Rename pattern", |p| {
            if let Some(pt) = p.pattern_mut(pid) {
                pt.name = name;
            }
        });
    }
    row.cut_left(if compact { 4.0 } else { 8.0 });
    let add = ui.icon_button(
        W::new("pattern-add"),
        row.cut_left(30.0),
        icons::PLUS,
        false,
        None,
    );
    ui.tooltip(&add, "New pattern");
    if add.clicked {
        let id = new_pattern(studio, None);
        studio.session.select_pattern(id);
    }
}

/// Creates a new pattern (optionally duplicating `from`). Returns its id.
pub fn new_pattern(studio: &mut Studio, from: Option<&Pattern>) -> Id {
    let n = studio.session.project().patterns.len();
    let id = Id::new();
    let pattern = match from {
        Some(src) => {
            let mut p = src.clone();
            p.id = id;
            p.name = format!("{} copy", src.name);
            for note in &mut p.notes {
                note.id = Id::new();
            }
            p
        }
        None => Pattern {
            expression: Vec::new(),
            id,
            name: format!("Pattern {}", n + 1),
            color: PALETTE[(n * 5 + 3) % PALETTE.len()],
            length: Time::beats(4),
            notes: vec![],
        },
    };
    let label = if from.is_some() {
        "Duplicate pattern"
    } else {
        "New pattern"
    };
    studio
        .session
        .doc
        .edit(label, |p| p.patterns.push(std::sync::Arc::new(pattern)));
    id
}

fn delete_pattern(studio: &mut Studio, pid: Id) {
    studio.session.doc.edit("Delete pattern", |p| {
        p.patterns.retain(|x| x.id != pid);
        for a in p.arrangements.iter_mut() {
            let a = std::sync::Arc::make_mut(a);
            a.clips.retain(|c| !matches!(c.content, peel_document::ClipContent::Pattern { pattern } if pattern == pid));
        }
    });
    let first = studio.session.project().patterns.first().map(|p| p.id);
    if let Some(f) = first {
        studio.session.select_pattern(f);
    }
}

fn export_panel(ui: &mut Ui, anchor: Rect, studio: &mut Studio) {
    let id = W::new("export-panel");
    if studio.ws.export_panel {
        studio.ws.export_panel = false;
        ui.open_popup(id, anchor);
    }
    if !ui.is_popup_open(id) {
        return;
    }
    let t = ui.theme.clone();
    let screen = ui.screen();
    let w = 440.0f32.min(screen.w - 16.0);
    let has_dither = studio.ws.export_format != AudioFormat::Mp3;
    let h = if has_dither { 400.0 } else { 364.0 };
    let r = Rect {
        x: (anchor.right() - w)
            .min(screen.right() - w - 8.0)
            .max(screen.x + 8.0),
        y: (anchor.bottom() + 6.0)
            .min(screen.bottom() - h - 8.0)
            .max(screen.y + 8.0),
        w,
        h,
    };
    let mut action = 0;
    ui.popup_panel(id, r, |ui, r| {
        let mut body = r.shrink(16.0);
        ui.text(
            body.cut_top(22.0),
            "Export",
            t.heading,
            t.text,
            Align::Start,
        );
        body.cut_top(10.0);
        let row = |body: &mut Rect| {
            let mut r = body.cut_top(28.0);
            body.cut_top(8.0);
            let label = r.cut_left(80.0);
            (label, r)
        };
        let (l, c) = row(&mut body);
        ui.text(l, "Format", t.caption, t.text_dim, Align::Start);
        let formats = [
            AudioFormat::Wav,
            AudioFormat::Mp3,
            AudioFormat::Flac,
            AudioFormat::Aiff,
        ];
        let mut i = formats
            .iter()
            .position(|f| *f == studio.ws.export_format)
            .unwrap_or(0);
        if ui.segmented(
            W::new("export-format"),
            c,
            &["WAV", "MP3", "FLAC", "AIFF"],
            &mut i,
        ) {
            studio.ws.export_format = formats[i];
            if studio.ws.export_format != AudioFormat::Wav && studio.ws.export_bits == 32 {
                studio.ws.export_bits = 24;
            }
            if !studio
                .ws
                .export_format
                .supported_sample_rates()
                .contains(&studio.ws.export_rate)
            {
                studio.ws.export_rate = 48_000;
            }
        }
        ui.describe_node(
            W::new("export-format"),
            peel_ui::Semantics::patch().label("Audio format"),
        );
        let (l, c) = row(&mut body);
        if studio.ws.export_format == AudioFormat::Mp3 {
            ui.text(l, "kbps", t.caption, t.text_dim, Align::Start);
            let bitrates = [128, 192, 256, 320];
            let mut i = bitrates
                .iter()
                .position(|b| *b == studio.ws.export_bitrate)
                .unwrap_or(3);
            if ui.segmented(
                W::new("export-bitrate"),
                c,
                &["128", "192", "256", "320"],
                &mut i,
            ) {
                studio.ws.export_bitrate = bitrates[i];
            }
            ui.describe_node(
                W::new("export-bitrate"),
                peel_ui::Semantics::patch().label("MP3 bitrate in kilobits per second"),
            );
        } else {
            ui.text(l, "Depth", t.caption, t.text_dim, Align::Start);
            let bits: &[u16] = if studio.ws.export_format == AudioFormat::Wav {
                &[16, 24, 32]
            } else {
                &[16, 24]
            };
            let labels: &[&str] = if studio.ws.export_format == AudioFormat::Wav {
                &["16-bit", "24-bit", "32 float"]
            } else {
                &["16-bit", "24-bit"]
            };
            let mut i = bits
                .iter()
                .position(|b| *b == studio.ws.export_bits)
                .unwrap_or(1);
            if ui.segmented(W::new("export-bits"), c, labels, &mut i) {
                studio.ws.export_bits = bits[i];
            }
            ui.describe_node(
                W::new("export-bits"),
                peel_ui::Semantics::patch().label("Bit depth"),
            );
        }
        let (l, c) = row(&mut body);
        ui.text(l, "Rate (kHz)", t.caption, t.text_dim, Align::Start);
        let rates = studio.ws.export_format.supported_sample_rates();
        let labels: Vec<String> = rates
            .iter()
            .map(|rate| format!("{}", *rate as f32 / 1000.0))
            .collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut i = rates
            .iter()
            .position(|x| *x == studio.ws.export_rate)
            .unwrap_or(1);
        if ui.segmented(W::new("export-rate"), c, &labels, &mut i) {
            studio.ws.export_rate = rates[i];
        }
        ui.describe_node(
            W::new("export-rate"),
            peel_ui::Semantics::patch().label("Sample rate in kilohertz"),
        );
        let (l, c) = row(&mut body);
        ui.text(l, "Channels", t.caption, t.text_dim, Align::Start);
        let mut i = usize::from(studio.ws.export_mono);
        if ui.segmented(W::new("export-channels"), c, &["Stereo", "Mono"], &mut i) {
            studio.ws.export_mono = i == 1;
        }
        ui.describe_node(
            W::new("export-channels"),
            peel_ui::Semantics::patch().label("Output channels"),
        );
        let (l, c) = row(&mut body);
        ui.text(l, "Tail", t.caption, t.text_dim, Align::Start);
        let tails = [0.0f32, 3.0, 8.0];
        let mut i = tails
            .iter()
            .position(|x| *x == studio.ws.export_tail)
            .unwrap_or(1);
        if ui.segmented(W::new("export-tail"), c, &["None", "3 s", "8 s"], &mut i) {
            studio.ws.export_tail = tails[i];
        }
        ui.describe_node(
            W::new("export-tail"),
            peel_ui::Semantics::patch().label("Effect tail"),
        );
        let (l, c) = row(&mut body);
        ui.text(l, "Files", t.caption, t.text_dim, Align::Start);
        let mut i = usize::from(studio.ws.export_stems);
        if ui.segmented(W::new("export-stems"), c, &["Mix", "Stems"], &mut i) {
            studio.ws.export_stems = i == 1;
        }
        ui.describe_node(
            W::new("export-stems"),
            peel_ui::Semantics::patch().label("Export files"),
        );
        let (l, c) = row(&mut body);
        ui.text(l, "Normalize", t.caption, t.text_dim, Align::Start);
        let mut i = usize::from(studio.ws.export_normalize);
        if ui.segmented(
            W::new("export-normalize"),
            c,
            &["Off", "-1 dB peak"],
            &mut i,
        ) {
            studio.ws.export_normalize = i == 1;
        }
        ui.describe_node(
            W::new("export-normalize"),
            peel_ui::Semantics::patch().label("Peak normalization, applied to each file"),
        );
        if studio.ws.export_format != AudioFormat::Mp3 {
            let (l, c) = row(&mut body);
            ui.text(l, "Dither", t.caption, t.text_dim, Align::Start);
            if studio.ws.export_bits == 32 {
                ui.text(
                    c,
                    "Not used for float audio",
                    t.caption,
                    t.text_dim,
                    Align::Start,
                );
            } else {
                let mut i = usize::from(studio.ws.export_dither);
                if ui.segmented(W::new("export-dither"), c, &["Off", "TPDF"], &mut i) {
                    studio.ws.export_dither = i == 1;
                }
                ui.describe_node(
                    W::new("export-dither"),
                    peel_ui::Semantics::patch().label("Dither"),
                );
            }
        }
        body.cut_top(6.0);
        let mut buttons = body.cut_top(34.0);
        let midi = buttons.cut_left(100.0);
        buttons.cut_left(8.0);
        let label = if studio.ws.export.is_some() {
            "Exporting…".to_string()
        } else if studio.ws.export_stems {
            "Export stems".to_string()
        } else {
            format!("Export {}", studio.ws.export_format.label())
        };
        if ui
            .button_primary(W::new("export-audio"), buttons, &label)
            .clicked
            && studio.ws.export.is_none()
        {
            action = 1;
        }
        ui.describe_node(
            W::new("export-audio"),
            peel_ui::Semantics::patch().disabled(studio.ws.export.is_some()),
        );
        if ui.button(W::new("export-midi"), midi, "MIDI").clicked {
            action = 2;
        }
    });
    match action {
        1 => {
            ui.close_popup();
            studio.export_audio();
        }
        2 => {
            ui.close_popup();
            studio.export_midi();
        }
        _ => {}
    }
}

/// Undo history: every step in musical terms; click one to return to it.
fn history_panel(ui: &mut Ui, anchor: Rect, studio: &mut Studio) {
    let id = W::new("history");
    if !ui.is_popup_open(id) {
        return;
    }
    let t = ui.theme.clone();
    let undo: Vec<String> = studio
        .session
        .doc
        .history_labels()
        .map(str::to_string)
        .collect();
    let redo: Vec<String> = studio
        .session
        .doc
        .redo_labels()
        .map(str::to_string)
        .collect();
    let row_h = 26.0;
    let shown_undo = undo.len().min(16);
    let shown_redo = redo.len().min(6);
    let h = 52.0 + (shown_undo + shown_redo + 1) as f32 * row_h;
    let r = Rect {
        x: (anchor.right() - 280.0).max(8.0),
        y: anchor.bottom() + 6.0,
        w: 280.0,
        h,
    };
    let mut jump: Option<i64> = None;
    ui.popup_panel(id, r, |ui, r| {
        let mut body = r.shrink(12.0);
        ui.text(
            body.cut_top(24.0),
            "History",
            t.heading,
            t.text,
            Align::Start,
        );
        body.cut_top(6.0);
        // Redo entries (greyed), newest at the top, then the current state,
        // then undo entries from most recent.
        for (i, label) in redo.iter().rev().take(shown_redo).enumerate().rev() {
            let row = body.cut_top(row_h);
            let resp = ui.interact(W::new(("history-redo", i)), row);
            if resp.hovered {
                ui.scene.rect(row, t.radius_s, t.control_hover);
            }
            ui.text(
                row.shrink2(10.0, 0.0),
                label,
                t.body,
                t.text_faint,
                Align::Start,
            );
            if resp.clicked {
                jump = Some(-(redo.len() as i64 - (redo.len() - 1 - i) as i64));
            }
        }
        let now = body.cut_top(row_h);
        ui.scene.rect(now, t.radius_s, t.accent.alpha(0.14));
        ui.text(
            now.shrink2(10.0, 0.0),
            "Now",
            t.label,
            t.accent,
            Align::Start,
        );
        for (i, label) in undo.iter().rev().take(shown_undo).enumerate() {
            let row = body.cut_top(row_h);
            let resp = ui.interact(W::new(("history-undo", i)), row);
            if resp.hovered {
                ui.scene.rect(row, t.radius_s, t.control_hover);
            }
            ui.text(row.shrink2(10.0, 0.0), label, t.body, t.text, Align::Start);
            if resp.clicked {
                jump = Some(i as i64 + 1);
            }
        }
    });
    if let Some(steps) = jump {
        // Positive: undo that many steps; negative: redo.
        if steps > 0 {
            for _ in 0..steps {
                studio.session.doc.undo();
            }
        } else {
            for _ in 0..(-steps) {
                studio.session.doc.redo();
            }
        }
        ui.close_popup();
    }
}
