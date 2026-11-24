//! Inspector: properties of the focused channel or mixer track, including
//! full device panels generated from parameter descriptors.

use peel_document::{DeviceInstance, Id};
use peel_dsp::DeviceDescriptor;
use peel_session::Session;
use peel_ui::Id as W;
use peel_ui::{Align, Color, MenuItem, Rect, Ui, icons, vec2};

use crate::device_layout::{self, knob_grid};
use crate::util::{col, db_to_fader, fader_to_db, format_db, note_name};
use crate::workspace::{Focus, Workspace};

/// Narrowest knob cell. Cells stretch to fill the row, so a 300 px
/// inspector lays out three 90 px columns: wide enough for names such as
/// "Hammer Noise" without truncation. Longer names still fit-and-tooltip.
pub(crate) const CELL_W: f32 = 76.0;
pub(crate) const CELL_H: f32 = 60.0;

#[derive(Default)]
struct WaveCache {
    asset: Option<Id>,
    cols: Vec<(f32, f32)>,
}

/// Draws the Inspector. Returns true when the user asked to hide it.
pub fn draw(ui: &mut Ui, r: Rect, s: &mut Session, ws: &mut Workspace) -> bool {
    let t = ui.theme.clone();
    ui.scene.rect(r, 0.0, t.panel);
    let mut body = r.shrink2(10.0, 0.0);
    let focus = match ws.focus {
        Some(Focus::Channel(id)) if s.project().channel(id).is_some() => Some(Focus::Channel(id)),
        Some(Focus::Track(id)) if s.project().mixer.track(id).is_some() => Some(Focus::Track(id)),
        Some(Focus::Clip(id)) if audio_clip_of(s, id).is_some() => Some(Focus::Clip(id)),
        _ => None,
    };
    // Pane header: what kind of thing is selected, and Hide.
    let head_full = body.cut_top(t.header_h);
    ui.scene
        .hline(r.x, r.right(), head_full.bottom() - 0.5, t.line);
    let mut head = head_full.shrink2(0.0, 5.0);
    let hide = ui.icon_button(
        W::new("inspector-hide"),
        head.cut_right(24.0),
        icons::CARET_DOUBLE_RIGHT,
        false,
        None,
    );
    ui.name(&hide, "Hide inspector");
    ui.tooltip(&hide, "Hide the inspector");
    let kind = match focus {
        Some(Focus::Channel(_)) => "Channel",
        Some(Focus::Track(id)) if s.project().mixer.tracks[0].id == id => "Master",
        Some(Focus::Track(_)) => "Mixer track",
        Some(Focus::Clip(_)) => "Audio clip",
        None => "Inspector",
    };
    ui.text(head, kind, t.label, t.text_dim, Align::Start);
    ui.describe_label(W::new("inspector-kind"), head, kind);
    body.cut_bottom(12.0);
    match focus {
        Some(Focus::Channel(id)) => channel(ui, body, id, s, ws),
        Some(Focus::Track(id)) => track(ui, body, id, s, ws),
        Some(Focus::Clip(id)) => audio_clip(ui, body, id, s),
        None => {
            // Restrained empty state: one line, plus a hint when hints are on.
            let mut c = body;
            c.cut_top(4.0);
            ui.text(
                c.cut_top(22.0),
                "Nothing selected",
                t.body,
                t.text_faint,
                Align::Start,
            );
            crate::util::hint(
                ui,
                W::new("inspector-empty-hint"),
                c,
                "Click a channel, mixer track or audio clip to edit it here.",
            );
        }
    }
    hide.clicked
}

/// The selected object's name block: a color bar, the name and a detail
/// line. Returns the rect left of the name for actions (right side).
fn object_head(ui: &mut Ui, r: Rect, color: Color) -> Rect {
    let mut hr = r;
    let bar = hr.cut_left(3.0);
    ui.scene.rect(bar.shrink2(0.0, 3.0), 1.5, color);
    hr.cut_left(10.0);
    hr
}

/// The label column of a property row (72 px).
fn prop_label(ui: &mut Ui, row: &mut Rect, label: &str) {
    let t = ui.theme.clone();
    let lr = row.cut_left(72.0);
    ui.text(lr, label, t.caption, t.text_faint, Align::Start);
}

fn scroll_body(ui: &mut Ui, id: W, r: Rect, content_h: f32) -> f32 {
    let mut off = vec2(0.0, *ui.state::<f32>(id));
    ui.scroll_area(id, r, vec2(r.w, content_h), &mut off);
    *ui.state::<f32>(id) = off.y;
    off.y
}

fn channel(ui: &mut Ui, r: Rect, id: Id, s: &mut Session, ws: &mut Workspace) {
    let t = ui.theme.clone();
    let Some(ch) = s.project().channel(id).cloned() else {
        return;
    };
    let desc = peel_devices::descriptor(&ch.device.type_id);
    let mut body = r;

    // Header: name, device and preset, and the channel's actions.
    let head = body.cut_top(44.0);
    let mut hr = object_head(ui, head, col(ch.color));
    let more_r = hr.cut_right(28.0).with_h(28.0);
    let more_id = W::new(("insp-channel-more", id));
    let more = ui.icon_button(
        more_id,
        more_r,
        icons::DOTS,
        ui.is_popup_open(more_id.with("menu")),
        None,
    );
    ui.name(&more, "Channel actions");
    ui.tooltip(&more, "Channel actions");
    hr.cut_right(4.0);
    let play = ui.icon_button(
        W::new(("insp-open-instrument", id)),
        hr.cut_right(28.0).with_h(28.0),
        icons::PIANO_KEYS,
        ws.inst.target == Some(id),
        None,
    );
    ui.name(&play, "Open instrument");
    ui.tooltip(&play, "Open the instrument: play it and edit every control");
    if play.clicked {
        crate::instrument_editor::open(s, ws, id);
    }
    if more.clicked {
        ui.open_popup(more_id.with("menu"), more_r);
    }
    hr.cut_right(4.0);
    let name_r = hr.cut_top(24.0);
    let name_id = W::new(("rename-channel", id));
    let items = [
        MenuItem::new("Rename"),
        MenuItem::new("Edit in piano roll").shortcut("F7"),
        MenuItem::new("Open instrument"),
        MenuItem::Separator,
        MenuItem::new("Delete channel"),
    ];
    match ui.menu(
        more_id.with("menu"),
        vec2(more_r.right() - 180.0, more_r.bottom() + 4.0),
        &items,
    ) {
        Some(0) => {
            *ui.state::<String>(name_id) = ch.name.clone();
            ui.request_focus(name_id);
        }
        Some(1) => {
            ws.selected_channel = Some(id);
            ws.dock = crate::workspace::DockTab::PianoRoll;
        }
        Some(2) => crate::instrument_editor::open(s, ws, id),
        Some(4) => {
            crate::rack::delete_channel(s, ws, id);
            return;
        }
        _ => {}
    }
    if ui.has_focus(name_id) {
        let mut name = ui.state::<String>(name_id).clone();
        let resp = ui.text_edit(name_id, name_r, &mut name, "Name");
        *ui.state::<String>(name_id) = name.clone();
        if resp.changed && !name.trim().is_empty() {
            let n = name.trim().to_string();
            s.doc.edit("Rename channel", |p| {
                if let Some(c) = p.channel_mut(id) {
                    c.name = n.clone();
                    let track = c.track;
                    if let Some(t) = p.mixer.track_mut(track) {
                        t.name = n;
                    }
                }
            });
        }
    } else {
        let resp = ui.interact(name_id.with("label"), name_r);
        let shown = ui.fit_text(&ch.name, t.heading, name_r.w);
        ui.text(name_r, &shown, t.heading, t.text, Align::Start);
        ui.tooltip(&resp, "Double-click to rename");
        if resp.double_clicked {
            *ui.state::<String>(name_id) = ch.name.clone();
            ui.request_focus(name_id);
        }
    }
    let dev_name = desc.map_or("Missing device", |d| d.name);
    ui.text(
        hr,
        &format!(
            "{dev_name}{}",
            ch.device
                .preset
                .as_ref()
                .map(|p| format!("  ·  {p}"))
                .unwrap_or_default()
        ),
        t.caption,
        t.text_faint,
        Align::Start,
    );
    body.cut_top(8.0);

    // Volume and pan, then property rows: output and step key.
    let mut strip = body.cut_top(70.0);
    let mut vol = db_to_fader(ch.gain_db);
    // Volume and pan line up with the device knob columns below.
    let (_, cell_w) = knob_grid(body.w);
    let vcell = strip.cut_left(cell_w);
    let v = knob_cell(
        ui,
        W::new(("insp-vol", id)),
        vcell,
        &mut vol,
        0.8,
        col(ch.color),
        false,
        "Volume",
        &format_db(ch.gain_db),
    );
    if v.changed {
        s.doc.edit_gesture(v.id.0, "Change channel volume", |p| {
            if let Some(c) = p.channel_mut(id) {
                c.gain_db = fader_to_db(vol);
            }
        });
    }
    let mut pan = (ch.pan + 1.0) * 0.5;
    let pcell = strip.cut_left(cell_w);
    let pan_txt =
        peel_dsp::ParamDescriptor::linear(0, "", "", "", -1.0, 1.0, 0.0, peel_dsp::Unit::Pan)
            .format(ch.pan);
    let pr = knob_cell(
        ui,
        W::new(("insp-pan", id)),
        pcell,
        &mut pan,
        0.5,
        t.text_dim,
        true,
        "Pan",
        &pan_txt,
    );
    if pr.changed {
        s.doc.edit_gesture(pr.id.0, "Change channel pan", |p| {
            if let Some(c) = p.channel_mut(id) {
                c.pan = (pan * 2.0 - 1.0).clamp(-1.0, 1.0);
            }
        });
    }
    if v.released || pr.released {
        s.doc.end_gesture();
    }
    body.cut_top(4.0);
    let mut route_row = body.cut_top(28.0);
    prop_label(ui, &mut route_row, "Output");
    let route_r = route_row;
    let track_name = s
        .project()
        .mixer
        .track(ch.track)
        .map_or("Master".to_string(), |t| t.name.clone());
    let rresp = ui.dropdown(
        W::new(("insp-route", id)),
        route_r,
        &track_name,
        "Mixer track",
    );
    ui.tooltip(&rresp, "Mixer track this channel plays through");
    if rresp.clicked {
        ui.open_popup(W::new(("insp-route-menu", id)), route_r);
    }
    let tracks: Vec<(Id, String)> = s
        .project()
        .mixer
        .tracks
        .iter()
        .map(|t| (t.id, t.name.clone()))
        .collect();
    let items: Vec<MenuItem> = tracks
        .iter()
        .map(|(tid, n)| MenuItem::new(n.clone()).checked(*tid == ch.track))
        .collect();
    if let Some(i) = ui.menu(
        W::new(("insp-route-menu", id)),
        vec2(route_r.x, route_r.bottom() + 4.0),
        &items,
    ) {
        let target = tracks[i].0;
        s.doc.edit("Route channel", |p| {
            if let Some(c) = p.channel_mut(id) {
                c.track = target;
            }
        });
    }
    body.cut_top(4.0);
    let mut key_row = body.cut_top(28.0);
    prop_label(ui, &mut key_row, "Step key");
    let key_r = key_row;
    let mut key = ch.root_key as f64;
    ui.scene.rect(key_r, t.radius_m, t.control);
    let kr = key_r.shrink2(8.0, 0.0);
    let k = ui.drag_number(
        W::new(("insp-root", id)),
        kr,
        &mut key,
        0.0,
        127.0,
        0.15,
        t.readout_small,
        t.text,
        |v| note_name(v.round() as u8),
    );
    if k.changed {
        let new = key.round() as u8;
        let old = ch.root_key;
        let pid = s.selected_pattern();
        s.doc.edit_gesture(k.id.0, "Change step key", |p| {
            if let Some(c) = p.channel_mut(id) {
                c.root_key = new;
            }
            // Steps follow the key so the grid keeps showing them.
            if let Some(pt) = p.pattern_mut(pid) {
                for n in pt.notes.iter_mut().filter(|n| {
                    n.channel == id && n.key == old && n.length == peel_document::Time::new(1, 4)
                }) {
                    n.key = new;
                }
            }
        });
    }
    ui.tooltip(
        &k,
        "Key the step sequencer plays. Drag or double-click to type",
    );
    if k.released {
        s.doc.end_gesture();
    }
    body.cut_top(12.0);

    let Some(desc) = desc else {
        ui.text(
            body.with_h(30.0),
            &format!("Device {} is not installed", ch.device.type_id),
            t.body,
            t.warn,
            Align::Start,
        );
        return;
    };
    device_panel(ui, body, &ch.device, desc, s, ws, col(ch.color));
}

fn track(ui: &mut Ui, r: Rect, id: Id, s: &mut Session, ws: &mut Workspace) {
    let t = ui.theme.clone();
    let Some(tr) = s.project().mixer.track(id).cloned() else {
        return;
    };
    let is_master = s.project().mixer.tracks[0].id == id;
    let mut body = r;
    let head = body.cut_top(44.0);
    let mut hr = object_head(ui, head, if is_master { t.accent } else { col(tr.color) });
    let name = ui.fit_text(&tr.name, t.heading, hr.w);
    ui.text(hr.cut_top(24.0), &name, t.heading, t.text, Align::Start);
    let sub = if tr.inserts.is_empty() {
        "No effects".to_string()
    } else {
        format!(
            "{} effect{}",
            tr.inserts.len(),
            if tr.inserts.len() == 1 { "" } else { "s" }
        )
    };
    ui.text(hr, &sub, t.caption, t.text_faint, Align::Start);
    body.cut_top(8.0);

    // Output routing (not for the master).
    if !is_master {
        let row = body.cut_top(28.0);
        let mut rr = row;
        prop_label(ui, &mut rr, "Output");
        let out_id = tr.output.unwrap_or(s.project().mixer.tracks[0].id);
        let out_name = s
            .project()
            .mixer
            .track(out_id)
            .map_or("Master".to_string(), |t| t.name.clone());
        let resp = ui.dropdown(W::new(("insp-output", id)), rr, &out_name, "Output");
        if resp.clicked {
            ui.open_popup(W::new(("insp-output-menu", id)), rr);
        }
        // Valid destinations: any track that does not (indirectly) feed this one.
        let p = s.project();
        let feeds_into = |candidate: peel_document::Id| -> bool {
            let mut cur = Some(candidate);
            let mut guard = 0;
            while let Some(c) = cur {
                if c == id {
                    return true;
                }
                if guard > 64 {
                    return true;
                }
                guard += 1;
                cur = p.mixer.track(c).and_then(|t| t.output).filter(|o| *o != c);
                if c == p.mixer.tracks[0].id {
                    break;
                }
            }
            false
        };
        let dests: Vec<(peel_document::Id, String)> = p
            .mixer
            .tracks
            .iter()
            .filter(|t| t.id != id && !feeds_into(t.id))
            .map(|t| (t.id, t.name.clone()))
            .collect();
        let items: Vec<MenuItem> = dests
            .iter()
            .map(|(d, n)| MenuItem::new(n.clone()).checked(*d == out_id))
            .collect();
        if let Some(i) = ui.menu(
            W::new(("insp-output-menu", id)),
            vec2(rr.x, rr.bottom() + 4.0),
            &items,
        ) {
            let dest = dests[i].0;
            let master = s.project().mixer.tracks[0].id;
            s.doc.edit("Route mixer track", |p| {
                if let Some(t) = p.mixer.track_mut(id) {
                    t.output = if dest == master { None } else { Some(dest) };
                }
            });
        }
        body.cut_top(10.0);
        sends_section(ui, &mut body, &tr, s);
    }

    if tr.inserts.is_empty() {
        let mut c = body;
        c.cut_top(8.0);
        ui.text(
            c.cut_top(22.0),
            "No effects",
            t.body,
            t.text_faint,
            Align::Start,
        );
        crate::util::hint(
            ui,
            W::new(("insp-fx-hint", id)),
            c,
            "Add effects from the Browser's Effects tab, or from an empty mixer slot.",
        );
        return;
    }
    // Stack every insert's panel in a scroll area.
    let heights: Vec<f32> = tr
        .inserts
        .iter()
        .map(|d| {
            let key = if peel_devices::accepts_sidechain(&d.type_id) {
                38.0
            } else {
                0.0
            };
            peel_devices::descriptor(&d.type_id)
                .map_or(60.0, |desc| panel_height(desc, body.w) + 40.0 + key)
        })
        .collect();
    let total: f32 = heights.iter().sum::<f32>() + 12.0;
    let scroll_id = W::new(("insp-track-scroll", id));
    let off = scroll_body(ui, scroll_id, body, total);
    ui.scene.push_clip(body);
    let mut y = body.y - off;
    for (dev, h) in tr.inserts.iter().zip(heights) {
        let card = Rect {
            x: body.x,
            y,
            w: body.w - 6.0,
            h: h - 8.0,
        };
        y += h;
        if card.bottom() < body.y || card.y > body.bottom() {
            continue;
        }
        let Some(desc) = peel_devices::descriptor(&dev.type_id) else {
            continue;
        };
        let mut c = card;
        let header = c.cut_top(28.0);
        let mut hr = header;
        let did = dev.id;
        let led = ui.led(
            W::new(("insp-bypass", did)),
            hr.cut_left(20.0),
            !dev.bypass,
            t.play,
        );
        ui.tooltip(&led, if dev.bypass { "Enable" } else { "Bypass" });
        if led.clicked {
            s.doc.edit(
                if dev.bypass {
                    "Enable effect"
                } else {
                    "Bypass effect"
                },
                |p| {
                    if let Some(d) = p
                        .mixer
                        .track_mut(id)
                        .and_then(|t| t.inserts.iter_mut().find(|d| d.id == did))
                    {
                        d.bypass = !d.bypass;
                    }
                },
            );
        }
        hr.cut_left(6.0);
        let x = ui.icon_button(
            W::new(("insp-remove", did)),
            hr.cut_right(26.0),
            icons::X,
            false,
            None,
        );
        ui.tooltip(&x, "Remove");
        if peel_devices::shows_reduction(&dev.type_id) {
            hr.cut_right(8.0);
            reduction_meter(ui, hr.cut_right(70.0), s, did);
        }
        if x.clicked {
            let name = desc.name;
            s.doc.edit(format!("Remove {name}"), |p| {
                if let Some(t) = p.mixer.track_mut(id) {
                    t.inserts.retain(|d| d.id != did);
                }
            });
        }
        ui.text(
            hr,
            desc.name,
            t.label,
            if dev.bypass { t.text_faint } else { t.text },
            Align::Start,
        );
        c.cut_top(4.0);
        if peel_devices::accepts_sidechain(&dev.type_id) {
            sidechain_row(ui, c.cut_top(30.0), s, id, dev);
            c.cut_top(8.0);
        }
        device_panel(ui, c, dev, desc, s, ws, t.accent);
        ui.scene
            .hline(card.x, card.right(), card.bottom() + 3.0, t.line);
    }
    ui.scene.pop_clip();
}

fn panel_height(desc: &'static DeviceDescriptor, w: f32) -> f32 {
    let mut h = 0.0;
    if !peel_devices::presets(desc.type_id).is_empty() {
        h += 38.0;
    }
    if desc.type_id == "peel.sampler" {
        h += 80.0;
    }
    if desc.type_id == "peel.fx.eq" {
        h += 140.0;
    }
    h + device_layout::layout(desc, w).height + 8.0
}

pub(crate) fn device_panel(
    ui: &mut Ui,
    r: Rect,
    dev: &DeviceInstance,
    desc: &'static DeviceDescriptor,
    s: &mut Session,
    ws: &mut Workspace,
    accent: Color,
) {
    let t = ui.theme.clone();
    let mut body = r;
    // Presets.
    let presets = peel_devices::presets(desc.type_id);
    if !presets.is_empty() {
        device_layout::preset_row(ui, body.cut_top(28.0), dev, desc, presets, s);
        body.cut_top(10.0);
    }
    if desc.type_id == "peel.sampler" {
        sample_view(ui, &mut body, dev, s);
    }
    if desc.type_id == "peel.fx.eq" {
        eq_view(ui, &mut body, dev, desc, s);
    }
    // CLAP と VST3 のプラグインは、自分の画面を別ウィンドウで開ける。
    if peel_session::is_plugin_type(desc.type_id) && s.plugin_has_editor(dev.id) {
        let mut row = body.cut_top(28.0);
        let label = if s.plugin_editor_open(dev.id) {
            "Show plugin window"
        } else {
            "Open plugin window"
        };
        let w = 180.0_f32.min(row.w);
        if ui
            .button(W::new(("plugin-editor", dev.id)), row.cut_left(w), label)
            .clicked
            && let Err(e) = s.open_plugin_editor(dev.id)
        {
            s.notify(e, true);
        }
        body.cut_top(10.0);
    }
    // 楽器には、サステインペダル（CC64）のオートメーションを作れる。
    let is_instrument = s.project().channels.iter().any(|c| c.device.id == dev.id);
    if is_instrument {
        let mut row = body.cut_top(28.0);
        let w = 180.0_f32.min(row.w);
        if ui
            .button(
                W::new(("automate-sustain", dev.id)),
                row.cut_left(w),
                "Automate sustain pedal",
            )
            .clicked
        {
            crate::playlist::create_automation(
                s,
                ws,
                peel_document::AutomationTarget::MidiCc {
                    device: dev.id,
                    cc: 64,
                },
                0.0,
            );
        }
        body.cut_top(10.0);
    }

    let mut content_h = panel_height(desc, body.w);
    if !presets.is_empty() {
        content_h -= 38.0;
    }
    if desc.type_id == "peel.sampler" {
        content_h -= 80.0;
    }
    if desc.type_id == "peel.fx.eq" {
        content_h -= 140.0;
    }
    let scroll_id = W::new(("insp-dev-scroll", dev.id));
    let off = if content_h > body.h {
        scroll_body(ui, scroll_id, body, content_h)
    } else {
        0.0
    };
    ui.scene.push_clip(body);
    // Cells stretch to fill the row, so labels get the room there is.
    // Wide panels flow groups side by side (see `device_layout`).
    let lay = device_layout::layout(desc, body.w);
    for pl in &lay.placed {
        let group = &lay.groups[pl.group];
        let gx = body.x + pl.x;
        let mut y = body.y - off + pl.y;
        // One rule per line: the line's first heading draws it across the
        // panel; the others sit at the same text position without one.
        let rule_w = if pl.line_start {
            body.w - 6.0 - pl.x
        } else {
            pl.w
        };
        let (hy, hh, first) = if pl.first || pl.line_start {
            (y, 26.0, pl.first)
        } else {
            (y + 6.0, 20.0, true)
        };
        ui.group_heading(
            Rect {
                x: gx,
                y: hy,
                w: rule_w,
                h: hh,
            },
            group.name,
            first,
        );
        y += device_layout::HEAD_H;
        for &p in &group.wide {
            let row = Rect {
                x: gx,
                y,
                w: pl.w,
                h: 28.0,
            };
            y += device_layout::WIDE_H;
            let value = dev.params.get(p.key).copied().unwrap_or(p.default);
            let mut sel = value.round().max(0.0) as usize;
            let short: Vec<&str> = p.choices.to_vec();
            // Segmented when every label fits its cell, otherwise a menu.
            let cell_w = (row.w - 72.0 - 4.0) / p.choices.len().max(1) as f32;
            let fits = p
                .choices
                .iter()
                .all(|c| ui.measure(c, t.label).x + 10.0 <= cell_w);
            if p.choices.len() <= 5 && fits {
                let mut rr = row;
                prop_label(ui, &mut rr, p.name);
                if ui.segmented(W::new(("choice", dev.id, p.id)), rr, &short, &mut sel) {
                    s.set_param(
                        dev.id,
                        p.key,
                        sel as f32,
                        W::new(("choice-g", dev.id, p.id, sel)).0,
                    );
                    s.doc.end_gesture();
                }
            } else {
                let id = W::new(("choice-menu", dev.id, p.id));
                let mut rr = row;
                prop_label(ui, &mut rr, p.name);
                let resp = ui.dropdown(id, rr, &s.param_text(dev.id, p, value), p.name);
                if resp.clicked {
                    ui.open_popup(id.with("m"), rr);
                }
                let items: Vec<MenuItem> = p
                    .choices
                    .iter()
                    .enumerate()
                    .map(|(i, c)| MenuItem::new(*c).checked(i == sel))
                    .collect();
                if let Some(i) = ui.menu(id.with("m"), vec2(rr.x, rr.bottom() + 4.0), &items) {
                    s.set_param(dev.id, p.key, i as f32, id.0 ^ i as u64);
                    s.doc.end_gesture();
                }
            }
        }
        for chunk in group.knobs.chunks(pl.cols.max(1)) {
            for (i, p) in chunk.iter().enumerate() {
                let cell = Rect {
                    x: gx + i as f32 * pl.cell_w,
                    y,
                    w: pl.cell_w,
                    h: CELL_H,
                };
                let value = dev.params.get(p.key).copied().unwrap_or(p.default);
                if p.is_choice() {
                    // Two-state switch.
                    let on = value >= 0.5;
                    let mut cr = cell;
                    let led_r = cr.cut_top(40.0);
                    let resp = ui.led(
                        W::new(("toggle", dev.id, p.id)),
                        Rect::from_center(led_r.center(), 22.0, 22.0),
                        on,
                        accent,
                    );
                    ui.text(
                        cr.cut_top(16.0),
                        p.name,
                        t.caption,
                        t.text_dim,
                        Align::Center,
                    );
                    ui.tooltip(&resp, &s.param_text(dev.id, p, value));
                    if resp.clicked {
                        s.set_param(
                            dev.id,
                            p.key,
                            if on { 0.0 } else { 1.0 },
                            W::new(("toggle-g", dev.id, p.id, on)).0,
                        );
                        s.doc.end_gesture();
                    }
                } else {
                    let mut norm = p.to_normalized(value);
                    let default = p.to_normalized(p.default);
                    let bipolar = p.min < 0.0 && p.max > 0.0;
                    let resp = knob_cell(
                        ui,
                        W::new(("param", dev.id, p.id)),
                        cell,
                        &mut norm,
                        default,
                        accent,
                        bipolar,
                        p.name,
                        &s.param_text(dev.id, p, value),
                    );
                    let menu = W::new(("param-menu", dev.id, p.id));
                    if resp.secondary_clicked {
                        ui.open_popup(menu, resp.rect);
                    }
                    let items = [
                        MenuItem::new("Create automation clip"),
                        MenuItem::new("Reset to default"),
                    ];
                    match ui.menu(menu, vec2(resp.rect.x, resp.rect.bottom()), &items) {
                        Some(0) => {
                            let target = peel_document::AutomationTarget::Param {
                                device: dev.id,
                                param: p.key.to_string(),
                            };
                            crate::playlist::create_automation(
                                s,
                                ws,
                                target,
                                p.to_normalized(value),
                            );
                        }
                        Some(1) => {
                            s.set_param(dev.id, p.key, p.default, menu.0);
                            s.doc.end_gesture();
                        }
                        _ => {}
                    }
                    if resp.changed {
                        let plain = p.from_normalized(norm);
                        s.set_param(dev.id, p.key, plain, resp.id.0);
                    }
                    if resp.released {
                        s.doc.end_gesture();
                    }
                }
            }
            y += CELL_H;
        }
    }
    ui.scene.pop_clip();
}

/// A knob with its name below; shows the value while hovered or dragged.
#[allow(clippy::too_many_arguments)]
pub(crate) fn knob_cell(
    ui: &mut Ui,
    id: W,
    cell: Rect,
    norm: &mut f32,
    default: f32,
    color: Color,
    bipolar: bool,
    name: &str,
    value: &str,
) -> peel_ui::Response {
    let t = ui.theme.clone();
    let mut c = cell;
    let kr = c.cut_top(40.0);
    let knob = Rect::from_center(kr.center(), 34.0, 34.0);
    let resp = ui.knob(id, knob, norm, default, color, bipolar);
    ui.name(&resp, name);
    ui.value_text(&resp, value);
    let label_r = c.cut_top(18.0);
    if resp.hovered || resp.active {
        ui.text(label_r, value, t.readout_small, t.text, Align::Center);
    } else {
        let n = ui.fit_text(name, t.caption, label_r.w - 2.0);
        ui.text(label_r, &n, t.caption, t.text_dim, Align::Center);
        if n != name {
            ui.tooltip(&resp, name);
        }
    }
    resp
}

fn sample_view(ui: &mut Ui, body: &mut Rect, dev: &DeviceInstance, s: &mut Session) {
    let t = ui.theme.clone();
    let r = body.cut_top(72.0);
    body.cut_top(8.0);
    ui.scene.rect(r, t.radius_m, t.bg);
    let Some(asset_id) = dev.sample else {
        if crate::util::help(ui) {
            let mut a = r;
            let b = a.cut_bottom(r.h * 0.5);
            ui.text(
                a.shrink2(0.0, 2.0),
                "No sample loaded",
                t.body,
                t.text_dim,
                Align::Center,
            );
            ui.text(
                b,
                "Drop an audio file on the Channel Rack",
                t.caption,
                t.text_faint,
                Align::Center,
            );
        } else {
            ui.text(r, "No sample loaded", t.body, t.text_faint, Align::Center);
        }
        return;
    };
    let name = s
        .project()
        .asset(asset_id)
        .map(|a| a.name.clone())
        .unwrap_or_default();
    let Some(buf) = s.sample(asset_id) else {
        ui.text(r, "Loading…", t.caption, t.text_faint, Align::Center);
        ui.request_repaint();
        return;
    };
    let cache_id = W::new(("wave", asset_id));
    let w = r.w.max(1.0) as usize;
    if ui.state::<WaveCache>(cache_id).asset != Some(asset_id)
        || ui.state::<WaveCache>(cache_id).cols.len() != w
    {
        let data = buf.left();
        let per = (data.len() / w.max(1)).max(1);
        let cols: Vec<(f32, f32)> = (0..w)
            .map(|i| {
                let s0 = (i * per).min(data.len());
                let s1 = ((i + 1) * per).min(data.len());
                data[s0..s1]
                    .iter()
                    .fold((0.0f32, 0.0f32), |(lo, hi), &x| (lo.min(x), hi.max(x)))
            })
            .collect();
        let c = ui.state::<WaveCache>(cache_id);
        c.asset = Some(asset_id);
        c.cols = cols;
    }
    let cols = ui.state::<WaveCache>(cache_id).cols.clone();
    let mid = r.center().y;
    let amp = r.h * 0.42;
    for (i, (lo, hi)) in cols.iter().enumerate() {
        let x = r.x + i as f32;
        let y0 = mid - hi * amp;
        let y1 = mid - lo * amp;
        ui.scene.rect(
            Rect {
                x,
                y: y0,
                w: 1.0,
                h: (y1 - y0).max(1.0),
            },
            0.0,
            t.accent.alpha(0.85),
        );
    }
    let label = format!("{name}  ·  {:.2} s", buf.duration_seconds());
    ui.text(
        Rect {
            x: r.x + 8.0,
            y: r.y + 4.0,
            w: r.w - 16.0,
            h: 16.0,
        },
        &label,
        t.caption,
        t.text_dim,
        Align::Start,
    );
    // Start marker: drag to choose where playback begins.
    let start = dev
        .params
        .get("sample.start")
        .copied()
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let x = r.x + r.w * start;
    if start > 0.0 {
        ui.scene
            .rect(Rect { w: x - r.x, ..r }, t.radius_m, t.bg.alpha(0.6));
    }
    let handle = Rect {
        x: x - 6.0,
        w: 12.0,
        ..r
    };
    let id = W::new(("sample-start", dev.id));
    let resp = ui.interact(id, handle);
    ui.name(&resp, "Sample start");
    let hot = resp.hovered || resp.active;
    ui.scene.rect(
        Rect {
            x: x - 1.0,
            w: 2.0,
            ..r
        },
        1.0,
        if hot { t.text } else { t.accent },
    );
    ui.scene.triangle(
        Rect {
            x: x - 5.0,
            y: r.y,
            w: 10.0,
            h: 7.0,
        },
        peel_ui::Dir::Down,
        0.5,
        if hot { t.text } else { t.accent },
    );
    if hot {
        ui.set_cursor(peel_ui::CursorIcon::EwResize);
    }
    ui.tooltip(&resp, &format!("Start {:.0}%", start * 100.0));
    if resp.dragging
        && let Some(p) = ui.pointer()
    {
        let v = ((p.x - r.x) / r.w).clamp(0.0, 0.99);
        s.set_param(dev.id, "sample.start", v, id.0);
    }
    if resp.released {
        s.doc.end_gesture();
    }
}

const EQ_DB: f32 = 18.0;
const BAND_COLORS: [u32; 4] = [0xEC5E48, 0xF5943A, 0x55C48E, 0x5E9FEE];

/// Interactive EQ curve: drag a band's handle to set its frequency and
/// gain, scroll over it to change Q, double-click to switch it on or off.
fn eq_view(
    ui: &mut Ui,
    body: &mut Rect,
    dev: &DeviceInstance,
    desc: &'static DeviceDescriptor,
    s: &mut Session,
) {
    let t = ui.theme.clone();
    let r = body.cut_top(132.0);
    body.cut_top(8.0);
    ui.scene.rect(r, t.radius_m, t.bg);
    let plot = r.shrink2(8.0, 10.0);
    let (f_lo, f_hi) = (20.0f32, 20_000.0f32);
    let x_of = |f: f32| plot.x + plot.w * (f / f_lo).ln() / (f_hi / f_lo).ln();
    let f_of = |x: f32| f_lo * (f_hi / f_lo).powf(((x - plot.x) / plot.w).clamp(0.0, 1.0));
    let y_of = |db: f32| plot.center().y - db.clamp(-EQ_DB, EQ_DB) / EQ_DB * plot.h * 0.5;
    let db_of = |y: f32| ((plot.center().y - y) / (plot.h * 0.5) * EQ_DB).clamp(-EQ_DB, EQ_DB);
    // Grid: decades and the 0 dB line.
    for f in [100.0, 1000.0, 10_000.0] {
        ui.scene.vline(x_of(f), plot.y, plot.bottom(), t.grid_beat);
    }
    for f in [50.0, 200.0, 500.0, 2000.0, 5000.0] {
        ui.scene.vline(x_of(f), plot.y, plot.bottom(), t.grid_step);
    }
    ui.scene.hline(plot.x, plot.right(), y_of(0.0), t.grid_bar);
    ui.scene
        .hline(plot.x, plot.right(), y_of(12.0), t.grid_step);
    ui.scene
        .hline(plot.x, plot.right(), y_of(-12.0), t.grid_step);
    for (f, label) in [(100.0, "100"), (1000.0, "1k"), (10_000.0, "10k")] {
        ui.text_at(
            vec2(x_of(f) + 3.0, plot.bottom() - 14.0),
            label,
            t.caption.size(10.0),
            t.text_faint,
        );
    }
    let params: Vec<f32> = desc
        .params
        .iter()
        .map(|p| dev.params.get(p.key).copied().unwrap_or(p.default))
        .collect();
    let sr = s.sample_rate().max(8000.0);
    let n = (plot.w / 3.0) as usize;
    let pts: Vec<peel_ui::Vec2> = (0..=n)
        .map(|i| {
            let x = plot.x + plot.w * i as f32 / n as f32;
            vec2(
                x,
                y_of(peel_effects::eq::eq_response_db(&params, sr, f_of(x))),
            )
        })
        .collect();
    ui.scene.push_clip(r);
    for w in pts.windows(2) {
        let y0 = y_of(0.0);
        let top = w[0].y.min(y0);
        let h = (w[0].y - y0).abs();
        ui.scene.rect(
            Rect {
                x: w[0].x,
                y: top,
                w: (w[1].x - w[0].x).max(1.0),
                h,
            },
            0.0,
            t.accent.alpha(0.08),
        );
    }
    ui.scene.polyline(&pts, 2.0, t.accent);
    // Band handles.
    for band in 1..=4u32 {
        let key = |k: &str| format!("band{band}.{k}");
        let get = |k: &str| {
            let k = key(k);
            dev.params
                .get(&k)
                .copied()
                .or_else(|| desc.param_by_key(&k).map(|p| p.default))
                .unwrap_or(0.0)
        };
        let (freq, gain, q, on) = (get("freq"), get("gain"), get("q"), get("enable") >= 0.5);
        let c = peel_ui::Color::hex(BAND_COLORS[band as usize - 1]);
        let center = vec2(x_of(freq.clamp(f_lo, f_hi)), y_of(gain));
        let id = W::new(("eq-band", dev.id, band));
        let resp = ui.interact(id, Rect::from_center(center, 18.0, 18.0));
        let hot = resp.hovered || resp.active;
        if on {
            ui.scene
                .circle_stroke(center, if hot { 7.5 } else { 6.0 }, c, 1.5, t.bg);
        } else {
            ui.scene.circle_stroke(center, 5.5, t.bg, 1.5, c.alpha(0.7));
        }
        ui.text(
            Rect::from_center(center, 12.0, 12.0),
            &band.to_string(),
            t.caption.size(9.5),
            if on { t.text_on_accent } else { c },
            Align::Center,
        );
        if hot {
            ui.set_cursor(peel_ui::CursorIcon::Grab);
            ui.tooltip(
                &resp,
                &format!("{:.0} Hz  {:+.1} dB  Q {:.2}", freq, gain, q),
            );
        }
        if resp.double_clicked {
            s.set_param(
                dev.id,
                &key("enable"),
                if on { 0.0 } else { 1.0 },
                id.0 ^ 0xe0,
            );
            s.doc.end_gesture();
        } else if resp.dragging
            && let Some(p) = ui.pointer()
        {
            if !on {
                s.set_param(dev.id, &key("enable"), 1.0, id.0);
            }
            s.set_param(dev.id, &key("freq"), f_of(p.x).round(), id.0);
            s.set_param(
                dev.id,
                &key("gain"),
                (db_of(p.y) * 10.0).round() / 10.0,
                id.0,
            );
        }
        if resp.released {
            s.doc.end_gesture();
        }
        if resp.hovered && ui.input.scroll.y != 0.0 {
            let nq = (q * (1.0 + ui.input.scroll.y * 0.004)).clamp(0.1, 18.0);
            ui.input.scroll.y = 0.0;
            s.set_param(dev.id, &key("q"), nq, id.0 ^ 0x51);
        }
    }
    ui.scene.pop_clip();
}

/// Post-fader sends of a mixer track: one knob per send plus an add menu.
fn sends_section(ui: &mut Ui, body: &mut Rect, tr: &peel_document::MixerTrack, s: &mut Session) {
    let t = ui.theme.clone();
    let id = tr.id;
    let mut head = body.cut_top(24.0);
    ui.text(
        head.cut_left(60.0),
        "Sends",
        t.caption,
        t.text_dim,
        Align::Start,
    );
    let add = ui.icon_button(
        W::new(("send-add", id)),
        head.cut_right(24.0),
        icons::PLUS,
        false,
        None,
    );
    ui.tooltip(&add, "Send to a bus");
    if add.clicked {
        ui.open_popup(W::new(("send-menu", id)), add.rect);
    }
    // Valid targets: not this track, not already sent to, and not anything
    // that feeds into this track (that would make a loop).
    let p = s.project();
    let targets: Vec<(peel_document::Id, String)> = p
        .mixer
        .tracks
        .iter()
        .skip(1)
        .filter(|x| x.id != id && !tr.sends.iter().any(|sd| sd.target == x.id))
        .filter(|x| !reaches(p, x.id, id))
        .map(|x| (x.id, x.name.clone()))
        .collect();
    let items: Vec<MenuItem> = if targets.is_empty() {
        vec![MenuItem::new("Add a mixer track first").enabled(false)]
    } else {
        targets
            .iter()
            .map(|(_, n)| MenuItem::new(n.clone()))
            .collect()
    };
    if let Some(i) = ui.menu(
        W::new(("send-menu", id)),
        vec2(add.rect.x - 120.0, add.rect.bottom() + 4.0),
        &items,
    ) && let Some((target, name)) = targets.get(i).cloned()
    {
        s.doc.edit(format!("Send to {name}"), |p| {
            if let Some(x) = p.mixer.track_mut(id) {
                x.sends.push(peel_document::MixerSend {
                    target,
                    gain_db: -6.0,
                });
            }
        });
    }
    if tr.sends.is_empty() {
        let hint = body.cut_top(20.0);
        ui.text(hint, "None", t.caption, t.text_faint, Align::Start);
        body.cut_top(8.0);
        return;
    }
    let names: Vec<String> = tr
        .sends
        .iter()
        .map(|sd| {
            s.project()
                .mixer
                .track(sd.target)
                .map_or("Missing".into(), |x| x.name.clone())
        })
        .collect();
    for (i, sd) in tr.sends.iter().enumerate() {
        let mut row = body.cut_top(40.0);
        let knob_r = row.cut_left(40.0);
        let mut norm = peel_document::db_to_fader(sd.gain_db);
        let k = ui.knob(
            W::new(("send", id, i)),
            Rect::from_center(knob_r.center(), 34.0, 34.0),
            &mut norm,
            peel_document::db_to_fader(-6.0),
            t.accent,
            false,
        );
        ui.tooltip(&k, &crate::util::format_db(sd.gain_db));
        if k.changed {
            let db = peel_document::fader_to_db(norm);
            s.doc.edit_gesture(k.id.0, "Change send level", |p| {
                if let Some(x) = p.mixer.track_mut(id)
                    && let Some(send) = x.sends.get_mut(i)
                {
                    send.gain_db = db;
                }
            });
        }
        if k.released {
            s.doc.end_gesture();
        }
        row.cut_left(8.0);
        let x = ui.icon_button(
            W::new(("send-remove", id, i)),
            row.cut_right(24.0).shrink2(0.0, 8.0),
            icons::X,
            false,
            None,
        );
        if x.clicked {
            s.doc.edit("Remove send", |p| {
                if let Some(t) = p.mixer.track_mut(id)
                    && i < t.sends.len()
                {
                    t.sends.remove(i);
                }
            });
        }
        let label = format!("{}  {}", names[i], crate::util::format_db(sd.gain_db));
        ui.text(row, &label, t.label, t.text, Align::Start);
    }
    body.cut_top(8.0);
}

/// True if audio from `from` can reach `to` through outputs and sends.
/// Gain reduction of a dynamics insert, drawn from the right edge leftwards
/// over 0 to 24 dB, held and released smoothly.
pub(crate) fn reduction_meter(ui: &mut Ui, r: Rect, s: &Session, device: Id) {
    let t = ui.theme.clone();
    let fall = 0.05f32.powf(ui.dt().clamp(0.0, 0.1));
    let now = crate::util::reduction_db(ui, s, device);
    let held = ui.state::<f32>(W::new(("gr-meter", device)));
    *held = (*held * fall).max(now);
    let db = *held;
    let bar = Rect::from_center(r.center(), r.w, 6.0);
    ui.scene.rect(bar, 3.0, t.bg);
    let f = (db / 24.0).clamp(0.0, 1.0);
    if f > 0.002 {
        ui.scene.rect(
            Rect {
                x: bar.right() - bar.w * f,
                w: bar.w * f,
                ..bar
            },
            3.0,
            t.warn,
        );
    }
    let resp = ui.interact(W::new(("gr-meter-hit", device)), r);
    ui.tooltip(&resp, &format!("Gain reduction {db:.1} dB"));
}

/// Chooses which mixer track keys a sidechain-capable insert.
pub(crate) fn sidechain_row(
    ui: &mut Ui,
    row: Rect,
    s: &mut Session,
    track: Id,
    dev: &DeviceInstance,
) {
    let t = ui.theme.clone();
    let mut rr = row;
    ui.text(
        rr.cut_left(70.0),
        "Sidechain",
        t.caption,
        t.text_dim,
        Align::Start,
    );
    let p = s.project();
    // Any other track that does not already depend on this one.
    let options: Vec<(Id, String)> = p
        .mixer
        .tracks
        .iter()
        .skip(1)
        .filter(|x| x.id != track && !reaches(p, track, x.id))
        .map(|x| (x.id, x.name.clone()))
        .collect();
    let current = dev
        .sidechain
        .and_then(|k| p.mixer.track(k))
        .map_or("None".to_string(), |x| x.name.clone());
    let id = W::new(("insp-sidechain", dev.id));
    let b = ui.interact_button(id, rr, &format!("Sidechain {current}"));
    ui.scene.rect(
        rr,
        t.radius_m,
        if b.hovered {
            t.control_hover
        } else {
            t.control
        },
    );
    let mut inner = rr.shrink2(10.0, 0.0);
    ui.icon(inner.cut_right(14.0), icons::CARET_DOWN, 11.0, t.text_dim);
    if dev.sidechain.is_some() {
        ui.icon(inner.cut_left(18.0), icons::LINK, 12.0, t.accent);
    }
    ui.text(
        inner,
        &current,
        t.label,
        if dev.sidechain.is_some() {
            t.text
        } else {
            t.text_dim
        },
        Align::Start,
    );
    ui.tooltip(&b, "Duck this track when another one plays");
    if b.clicked {
        ui.open_popup(id.with("menu"), rr);
    }
    let mut items = vec![MenuItem::new("None").checked(dev.sidechain.is_none())];
    items.extend(
        options
            .iter()
            .map(|(oid, n)| MenuItem::new(n.clone()).checked(dev.sidechain == Some(*oid))),
    );
    if let Some(i) = ui.menu(id.with("menu"), vec2(rr.x, rr.bottom() + 4.0), &items) {
        let key = if i == 0 { None } else { Some(options[i - 1].0) };
        let did = dev.id;
        s.doc.edit(
            if key.is_some() {
                "Set sidechain"
            } else {
                "Remove sidechain"
            },
            |p| {
                if let Some(d) = p
                    .mixer
                    .track_mut(track)
                    .and_then(|t| t.inserts.iter_mut().find(|d| d.id == did))
                {
                    d.sidechain = key;
                }
            },
        );
    }
}

pub(crate) fn reaches(
    p: &peel_document::Project,
    from: peel_document::Id,
    to: peel_document::Id,
) -> bool {
    let master = p.mixer.tracks[0].id;
    let mut stack = vec![from];
    let mut seen = std::collections::HashSet::new();
    while let Some(c) = stack.pop() {
        if c == to {
            return true;
        }
        if !seen.insert(c) || c == master {
            continue;
        }
        if p.mixer.track(c).is_some() {
            stack.extend(p.mixer.feeds(c));
        }
    }
    false
}

fn clip_source_bpm(c: &peel_document::Clip) -> Option<f64> {
    match &c.content {
        peel_document::ClipContent::Audio { source_bpm, .. } => *source_bpm,
        _ => None,
    }
}

/// The tempo of a loop of `seconds`: a whole number of bars (1 to 16) that
/// lands between 80 and 160 BPM, the one closest to the song's tempo.
fn guess_bpm(seconds: f64, song: f64) -> f64 {
    if seconds <= 0.0 {
        return song;
    }
    [1.0, 2.0, 4.0, 8.0, 16.0]
        .iter()
        .map(|bars: &f64| bars * 4.0 * 60.0 / seconds)
        .filter(|b| (80.0..160.0).contains(b))
        .min_by(|a, b| (a - song).abs().total_cmp(&(b - song).abs()))
        .map_or(song, |b| (b * 10.0).round() / 10.0)
}

/// Turns tempo following on (with a source tempo) or off. The clip keeps
/// playing the same part of the file, so its length, offset and fades are
/// rescaled between song beats and the loop's own beats.
fn set_follow(s: &mut Session, id: Id, bpm: Option<f64>, gesture: Option<u64>) {
    let tempo = peel_time::TempoMap::from_segments(&s.project().tempo);
    let label = match bpm {
        Some(_) => "Follow tempo",
        None => "Play at native speed",
    };
    let apply = |p: &mut peel_document::Project| {
        let Some(c) = p.arrangement_mut().clips.iter_mut().find(|c| c.id == id) else {
            return;
        };
        let song = tempo.bpm_at(c.start.as_f64());
        let peel_document::ClipContent::Audio {
            source_bpm,
            fade_in,
            fade_out,
            ..
        } = &mut c.content
        else {
            return;
        };
        // Beats per beat: how the clip's timing scales from the old mode to
        // the new one.
        let before = source_bpm.unwrap_or(song);
        let after = bpm.unwrap_or(song);
        let k = after / before;
        let scale = |t: peel_document::Time| peel_document::Time::from_f64(t.as_f64() * k, 960);
        *fade_in = scale(*fade_in);
        *fade_out = scale(*fade_out);
        *source_bpm = bpm;
        c.offset = scale(c.offset);
        c.length = scale(c.length).max(peel_document::Time::new(1, 16));
    };
    match gesture {
        Some(g) => s.doc.edit_gesture(g, label, apply),
        None => s.doc.edit(label, apply),
    }
}

fn audio_clip_of(s: &Session, id: Id) -> Option<peel_document::Clip> {
    s.project()
        .arrangement()
        .clips
        .iter()
        .find(|c| c.id == id && matches!(c.content, peel_document::ClipContent::Audio { .. }))
        .cloned()
}

/// Gain range of the clip gain knob, in dB.
const CLIP_GAIN: (f32, f32) = (-24.0, 12.0);

/// Changes an audio clip's settings as one undo step (or gesture).
fn edit_audio_clip(
    s: &mut Session,
    id: Id,
    label: &str,
    gesture: Option<u64>,
    f: impl FnOnce(&mut f32, &mut bool, &mut Id),
) {
    let apply = |p: &mut peel_document::Project| {
        if let Some(c) = p.arrangement_mut().clips.iter_mut().find(|c| c.id == id)
            && let peel_document::ClipContent::Audio {
                gain_db,
                reversed,
                track,
                ..
            } = &mut c.content
        {
            f(gain_db, reversed, track);
        }
    };
    match gesture {
        Some(g) => s.doc.edit_gesture(g, label, apply),
        None => s.doc.edit(label, apply),
    }
}

fn audio_clip(ui: &mut Ui, r: Rect, id: Id, s: &mut Session) {
    let t = ui.theme.clone();
    let Some(clip) = audio_clip_of(s, id) else {
        return;
    };
    let peel_document::ClipContent::Audio {
        asset,
        gain_db,
        reversed,
        track,
        ..
    } = clip.content.clone()
    else {
        return;
    };
    let name = s
        .project()
        .asset(asset)
        .map_or("Audio".to_string(), |a| a.name.clone());
    let tempo = peel_time::TempoMap::from_segments(&s.project().tempo);
    let bpm = tempo.bpm_at(clip.start.as_f64());
    let secs = clip.length.as_f64() * 60.0 / bpm;
    let mut body = r;

    // Header.
    let head = body.cut_top(44.0);
    let mut hr = object_head(ui, head, t.text_dim);
    let shown = ui.fit_text(&name, t.heading, hr.w);
    ui.text(hr.cut_top(24.0), &shown, t.heading, t.text, Align::Start);
    ui.text(
        hr,
        &format!("Audio clip  ·  {secs:.2} s"),
        t.caption,
        t.text_faint,
        Align::Start,
    );
    body.cut_top(10.0);

    // Whole file, with the part the clip plays lit.
    let wave = body.cut_top(72.0);
    body.cut_top(12.0);
    ui.scene.rect(wave, t.radius_m, t.bg);
    let total = s.asset_duration_seconds(asset).unwrap_or(0.0);
    // Where the clip reads the file, in the file's own seconds (a clip
    // following the song tempo counts beats at its source tempo).
    let src_bpm = clip_source_bpm(&clip).unwrap_or(bpm);
    let src_start = clip.offset.as_f64() * 60.0 / src_bpm;
    let src_secs = clip.length.as_f64() * 60.0 / src_bpm;
    if let Some(peaks) = s.audio_peaks(asset)
        && total > 0.0
    {
        let inner = wave.shrink2(6.0, 6.0);
        let cols = inner.w.max(1.0) as usize;
        let frames = peaks.frames();
        let (lit0, lit1) = (
            ((src_start / total) as f32).clamp(0.0, 1.0),
            (((src_start + src_secs) / total) as f32).clamp(0.0, 1.0),
        );
        for (i, (lo, hi)) in peaks.query(0, frames, cols).enumerate() {
            let x = inner.x + i as f32;
            let f = i as f32 / cols as f32;
            let c = if f >= lit0 && f <= lit1 {
                t.accent
            } else {
                t.text_faint.alpha(0.5)
            };
            let mid = inner.center().y;
            let (y0, y1) = (
                mid - hi.max(0.0) * inner.h * 0.5,
                mid - lo.min(0.0) * inner.h * 0.5,
            );
            ui.scene.rect(
                Rect {
                    x,
                    y: y0,
                    w: 1.0,
                    h: (y1 - y0).max(1.0),
                },
                0.0,
                c,
            );
        }
        let lit = Rect {
            x: inner.x + inner.w * lit0,
            w: inner.w * (lit1 - lit0).max(0.0),
            ..wave
        };
        ui.scene.outline(lit, 2.0, 1.0, t.accent.alpha(0.5));
    } else {
        ui.text(wave, "Loading", t.caption, t.text_faint, Align::Center);
    }

    // Gain, reverse, normalize.
    let row = body.cut_top(CELL_H);
    body.cut_top(8.0);
    let cells = row.columns(3, 0.0);
    let mut norm = (gain_db - CLIP_GAIN.0) / (CLIP_GAIN.1 - CLIP_GAIN.0);
    let default = -CLIP_GAIN.0 / (CLIP_GAIN.1 - CLIP_GAIN.0);
    let gid = W::new(("clip-gain", id));
    let k = knob_cell(
        ui,
        gid,
        cells[0],
        &mut norm,
        default,
        t.accent,
        false,
        "Gain",
        &crate::util::format_db(gain_db),
    );
    if k.changed {
        let db = (CLIP_GAIN.0 + norm * (CLIP_GAIN.1 - CLIP_GAIN.0)).clamp(CLIP_GAIN.0, CLIP_GAIN.1);
        edit_audio_clip(s, id, "Clip gain", Some(gid.0), |g, _, _| *g = db);
    }
    if k.released {
        s.doc.end_gesture();
    }
    let mut cr = cells[1];
    let led_r = cr.cut_top(44.0);
    let led = ui.led(
        W::new(("clip-rev", id)),
        Rect::from_center(led_r.center(), 22.0, 22.0),
        reversed,
        t.accent,
    );
    ui.name(&led, "Reverse");
    ui.text(
        cr.cut_top(16.0),
        "Reverse",
        t.caption,
        t.text_dim,
        Align::Center,
    );
    if led.clicked {
        edit_audio_clip(
            s,
            id,
            if reversed {
                "Play forwards"
            } else {
                "Reverse clip"
            },
            None,
            |_, rv, _| *rv = !*rv,
        );
    }
    let mut cr = cells[2];
    let btn_r = Rect::from_center(cr.cut_top(44.0).center(), 30.0, 30.0);
    let nb = ui.icon_button(
        W::new(("clip-normalize", id)),
        btn_r,
        icons::LIGHTNING,
        false,
        None,
    );
    ui.text(
        cr.cut_top(16.0),
        "Normalize",
        t.caption,
        t.text_dim,
        Align::Center,
    );
    ui.tooltip(
        &nb,
        "Set the gain so the loudest peak in the clip reaches 0 dB",
    );
    if nb.clicked
        && let Some(buf) = s.sample(asset)
    {
        let rate = f64::from(buf.sample_rate);
        let a = (src_start * rate) as usize;
        let b = ((src_start + src_secs) * rate) as usize;
        let peak = buf
            .channels
            .iter()
            .flat_map(|d| {
                let (a, b) = (a.min(d.len()), b.min(d.len()));
                d[a..b].iter().map(|x| x.abs())
            })
            .fold(0.0f32, f32::max);
        if peak > 1e-5 {
            let db = (-peel_dsp::util::gain_to_db(peak)).clamp(CLIP_GAIN.0, CLIP_GAIN.1);
            edit_audio_clip(s, id, "Normalize clip", None, |g, _, _| *g = db);
        } else {
            s.notify("The clip is silent", true);
        }
    }

    // Tempo: follow the song by time-stretching from the loop's own tempo.
    let mut row = body.cut_top(30.0);
    body.cut_top(10.0);
    ui.text(
        row.cut_left(60.0),
        "Tempo",
        t.caption,
        t.text_dim,
        Align::Start,
    );
    let follow = clip_source_bpm(&clip);
    let led = ui.led(
        W::new(("clip-follow", id)),
        Rect::from_center(row.cut_left(26.0).center(), 20.0, 20.0),
        follow.is_some(),
        t.accent,
    );
    ui.name(&led, "Follow song tempo");
    ui.tooltip(
        &led,
        "Stretch the audio to follow the song's tempo, keeping its pitch",
    );
    row.cut_left(6.0);
    if let Some(src) = follow {
        let mut v = src;
        let num = row.cut_left(110.0);
        ui.scene.rect(num, t.radius_m, t.control);
        let resp = ui.drag_number(
            W::new(("clip-bpm", id)),
            num,
            &mut v,
            40.0,
            240.0,
            0.1,
            t.label,
            t.text,
            |x| format!("{x:.1} BPM"),
        );
        if resp.changed {
            set_follow(
                s,
                id,
                Some(((v * 10.0).round() / 10.0).clamp(40.0, 240.0)),
                Some(W::new(("clip-bpm-g", id)).0),
            );
        }
        if resp.released {
            s.doc.end_gesture();
        }
        if s.stretching() {
            row.cut_left(8.0);
            ui.text(row, "Stretching", t.caption, t.text_faint, Align::Start);
        }
    } else {
        ui.text(row, "Native speed", t.caption, t.text_faint, Align::Start);
    }
    if led.clicked {
        let guess = follow.is_none().then(|| guess_bpm(total, bpm));
        set_follow(s, id, guess, None);
    }

    // Mixer output.
    let mut row = body.cut_top(28.0);
    ui.text(
        row.cut_left(60.0),
        "Output",
        t.caption,
        t.text_dim,
        Align::Start,
    );
    let tracks: Vec<(Id, String)> = s
        .project()
        .mixer
        .tracks
        .iter()
        .map(|m| (m.id, m.name.clone()))
        .collect();
    let current = tracks
        .iter()
        .find(|(tid, _)| *tid == track)
        .map_or("Master".to_string(), |x| x.1.clone());
    let oid = W::new(("clip-output", id));
    let ob = ui.interact_button(oid, row, &current);
    ui.scene.rect(
        row,
        t.radius_m,
        if ob.hovered {
            t.control_hover
        } else {
            t.control
        },
    );
    let mut inner = row.shrink2(10.0, 0.0);
    ui.icon(inner.cut_right(14.0), icons::CARET_DOWN, 11.0, t.text_dim);
    ui.text(inner, &current, t.label, t.text, Align::Start);
    if ob.clicked {
        ui.open_popup(oid.with("menu"), row);
    }
    let items: Vec<MenuItem> = tracks
        .iter()
        .map(|(tid, n)| MenuItem::new(n.clone()).checked(*tid == track))
        .collect();
    if let Some(i) = ui.menu(oid.with("menu"), vec2(row.x, row.bottom() + 4.0), &items) {
        let to = tracks[i].0;
        edit_audio_clip(s, id, "Route clip", None, |_, _, tr| *tr = to);
    }
}
