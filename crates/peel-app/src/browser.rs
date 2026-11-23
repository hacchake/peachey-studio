//! Browser: native instruments and effects, sample files and patterns.
//! Everything can be dragged onto the rack, mixer or playlist.
//!
//! Clicking an instrument or one of its presets plays it; double-clicking,
//! the plus button or a drag adds it. An effect has nothing to play on its
//! own, so a click adds it.

use std::path::{Path, PathBuf};

use peel_document::Id;
use peel_dsp::DeviceKind;
use peel_session::Session;
use peel_ui::Id as W;
use peel_ui::{Align, Color, CursorIcon, DragPayload, Rect, Ui, icons, vec2};

use crate::rack;
use crate::util::col;
use crate::workspace::{BrowserTab, DockTab, Focus, Workspace};

/// Payload for in-app drag and drop.
#[derive(Clone, Debug)]
pub enum BrowserItem {
    Device(&'static str),
    /// A device type with a named factory preset.
    Preset(&'static str, &'static str),
    Sample(PathBuf),
    Pattern(Id),
    /// An installed CLAP plugin (`clap:<id>`).
    Plugin {
        type_id: String,
        name: String,
        instrument: bool,
    },
}

const ROW_H: f32 = 24.0;

#[derive(Default)]
struct DirCache {
    dir: Option<PathBuf>,
    dirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

fn category_color(cat: &str) -> Color {
    let c = match cat {
        "Drums" => peel_document::PALETTE[0],
        "Bass" => peel_document::PALETTE[1],
        "Percussion" => peel_document::PALETTE[10],
        "Synth" => peel_document::PALETTE[6],
        "Keys" => peel_document::PALETTE[4],
        "Guitar" => peel_document::PALETTE[9],
        "Strings" => peel_document::PALETTE[5],
        "Wind" => peel_document::PALETTE[8],
        "Sampler" => peel_document::PALETTE[2],
        "EQ" => peel_document::PALETTE[5],
        "Dynamics" => peel_document::PALETTE[1],
        "Delay" | "Reverb" => peel_document::PALETTE[7],
        "Distortion" => peel_document::PALETTE[9],
        "Filter" | "Modulation" => peel_document::PALETTE[3],
        "Creative" => peel_document::PALETTE[8],
        _ => peel_document::PALETTE[11],
    };
    col(c)
}

fn category_icon(cat: &str) -> &'static str {
    match cat {
        "Drums" => icons::LIGHTNING,
        "Bass" => icons::SPEAKER,
        "Percussion" => icons::BELL,
        "Synth" => icons::WAVE_SAW,
        "Keys" => icons::PIANO_KEYS,
        "Guitar" => icons::MUSIC_NOTE,
        "Strings" => icons::MUSIC_NOTES,
        "Wind" => icons::WAVE_SINE,
        "Sampler" => icons::WAVEFORM,
        "EQ" => icons::EQUALIZER,
        "Dynamics" => icons::SLIDERS,
        "Delay" => icons::REPEAT,
        "Reverb" => icons::DROP,
        "Distortion" => icons::LIGHTNING,
        "Filter" => icons::WAVE_SINE,
        "Modulation" => icons::WAVE_TRIANGLE,
        "Creative" => icons::WAVE_SQUARE,
        _ => icons::SLIDERS,
    }
}

/// Draws the browser. Returns true when the user asked to hide it.
pub fn draw(ui: &mut Ui, r: Rect, s: &mut Session, ws: &mut Workspace) -> bool {
    let t = ui.theme.clone();
    ui.scene.rect(r, 0.0, t.panel);
    let mut body = r.shrink2(10.0, 0.0);
    // Header: the tab's name (tabs live on the left rail) and Hide.
    let head_full = body.cut_top(t.header_h);
    ui.scene
        .hline(r.x, r.right(), head_full.bottom() - 0.5, t.line);
    let mut head = head_full.shrink2(0.0, 5.0);
    let hide = ui.icon_button(
        W::new("browser-hide"),
        head.cut_right(24.0),
        icons::CARET_DOUBLE_LEFT,
        false,
        None,
    );
    ui.name(&hide, "Hide browser");
    ui.tooltip(&hide, "Hide the browser");
    let title = crate::layout::BROWSER_TABS
        .iter()
        .find(|(tab, _, _)| *tab == ws.browser_tab)
        .map_or("Browser", |(_, _, label)| *label);
    ui.text(head, title, t.label, t.text, Align::Start);
    ui.describe_label(W::new("browser-title"), head, title);
    body.cut_top(6.0);
    let search_r = body.cut_top(26.0);
    let placeholder = match ws.browser_tab {
        BrowserTab::Instruments => "Search instruments",
        BrowserTab::Effects => "Search effects",
        BrowserTab::Samples => "Search this folder",
        BrowserTab::Patterns => "Search patterns",
    };
    ui.text_edit(
        W::new("browser-search"),
        search_r,
        &mut ws.search,
        placeholder,
    );
    body.cut_top(4.0);
    let query = ws.search.to_lowercase();
    match ws.browser_tab {
        BrowserTab::Instruments => devices(ui, body, DeviceKind::Instrument, &query, s, ws),
        BrowserTab::Effects => devices(ui, body, DeviceKind::Effect, &query, s, ws),
        BrowserTab::Samples => samples(ui, body, &query, s, ws),
        BrowserTab::Patterns => patterns(ui, body, &query, s, ws),
    }
    hide.clicked
}

#[derive(Default)]
struct Expanded(std::collections::HashSet<&'static str>);

/// The instrument row last clicked: the one that was heard.
#[derive(Default)]
struct Picked {
    row: Option<W>,
    /// A double-click added the row on its second press; the click that
    /// ends it is not another request to hear it.
    added: bool,
}

/// What a press on an instrument row asks for.
enum Pick {
    Nothing,
    Hear,
    Add,
}

/// Reads an instrument row: a click hears it and marks it picked, and a
/// double-click or its add button adds it.
fn pick(ui: &mut Ui, picked_id: W, row: W, resp: &peel_ui::Response, add: bool) -> Pick {
    let st = ui.state::<Picked>(picked_id);
    if add || resp.double_clicked {
        st.added = resp.double_clicked;
        return Pick::Add;
    }
    if !resp.clicked {
        return Pick::Nothing;
    }
    if std::mem::take(&mut st.added) {
        return Pick::Nothing;
    }
    st.row = Some(row);
    Pick::Hear
}

/// Width of the add button at the right of an instrument row.
const ADD_W: f32 = 22.0;

/// The add button of an instrument row. It is drawn while the row is hot or
/// picked, and can always be reached with the keyboard.
fn add_button(ui: &mut Ui, id: W, r: Rect, name: &str) -> peel_ui::Response {
    let resp = ui.interact_button(id, r, &format!("Add {name}"));
    ui.tooltip(&resp, "Add to the Channel Rack");
    if resp.hovered {
        ui.set_cursor(CursorIcon::Pointer);
    }
    resp
}

enum Row {
    Heading(&'static str),
    Device(&'static peel_dsp::DeviceDescriptor, bool),
    Preset(&'static peel_dsp::DeviceDescriptor, &'static str),
    /// Index into the plugin list.
    Plugin(usize),
    Scanning,
    NoPlugins,
}

/// An installed plugin as the browser lists it.
struct PluginRow {
    type_id: String,
    name: String,
    vendor: String,
}

fn devices(
    ui: &mut Ui,
    r: Rect,
    kind: DeviceKind,
    query: &str,
    s: &mut Session,
    ws: &mut Workspace,
) {
    let t = ui.theme.clone();
    let expanded_id = W::new("browser-expanded");
    let picked_id = W::new("browser-picked");
    if let Ok(t) = std::env::var("PEEL_EXPAND")
        && let Some(d) = peel_devices::descriptor(&t)
    {
        ui.state::<Expanded>(expanded_id).0.insert(d.type_id);
    }
    let mut list: Vec<&'static peel_dsp::DeviceDescriptor> =
        peel_devices::all().filter(|d| d.kind == kind).collect();
    list.sort_by_key(|d| d.category);
    // Build rows; a search matches devices, categories and preset names.
    let mut rows: Vec<Row> = Vec::new();
    let mut last = "";
    for d in &list {
        let presets = peel_devices::presets(d.type_id);
        let name_hit = query.is_empty()
            || d.name.to_lowercase().contains(query)
            || d.category.to_lowercase().contains(query);
        let preset_hits: Vec<&'static str> = presets
            .iter()
            .map(|p| p.name)
            .filter(|n| !query.is_empty() && n.to_lowercase().contains(query))
            .collect();
        if !name_hit && preset_hits.is_empty() {
            continue;
        }
        if d.category != last {
            rows.push(Row::Heading(d.category));
            last = d.category;
        }
        let open =
            !preset_hits.is_empty() || ui.state::<Expanded>(expanded_id).0.contains(d.type_id);
        rows.push(Row::Device(d, open));
        if open {
            let shown: Vec<&'static str> = if preset_hits.is_empty() {
                presets.iter().map(|p| p.name).collect()
            } else {
                preset_hits
            };
            for p in shown {
                rows.push(Row::Preset(d, p));
            }
        }
    }
    // Third-party plugins after the built-in devices.
    let instrument = kind == DeviceKind::Instrument;
    let plugins: Vec<PluginRow> = s
        .plugins()
        .iter()
        .filter(|p| p.is_instrument() == instrument)
        .filter(|p| {
            query.is_empty()
                || p.name.to_lowercase().contains(query)
                || p.vendor.to_lowercase().contains(query)
        })
        .map(|p| PluginRow {
            type_id: peel_session::plugin_type_id(p),
            name: p.name.clone(),
            vendor: p.vendor.clone(),
        })
        .collect();
    if !plugins.is_empty() {
        rows.push(Row::Heading("Plugins"));
        rows.extend((0..plugins.len()).map(Row::Plugin));
    }
    // With none installed, one quiet line instead of a section.
    if query.is_empty() {
        if s.plugins_scanning() {
            rows.push(Row::Scanning);
        } else if plugins.is_empty() {
            rows.push(Row::NoPlugins);
        }
    }
    if rows.is_empty() {
        ui.text(
            r.with_h(40.0),
            "Nothing matches",
            t.body,
            t.text_faint,
            Align::Center,
        );
        return;
    }
    let content_h = rows.len() as f32 * ROW_H + 8.0;
    let mut off = vec2(0.0, ws.browser_scroll);
    ui.scroll_area(W::new("browser-scroll"), r, vec2(r.w, content_h), &mut off);
    ws.browser_scroll = off.y;
    ui.scene.push_clip(r);
    let mut y = r.y - off.y;
    for row_kind in rows {
        let row = Rect {
            x: r.x,
            y,
            w: r.w - 8.0,
            h: ROW_H,
        };
        y += ROW_H;
        if row.bottom() < r.y || row.y > r.bottom() {
            continue;
        }
        match row_kind {
            Row::Heading(h) => {
                let mut hr = Rect {
                    y: row.y + 6.0,
                    h: row.h - 6.0,
                    ..row.shrink2(6.0, 0.0)
                };
                if h == "Plugins" && !s.plugins_scanning() {
                    let b = ui.icon_button(
                        W::new("browser-rescan"),
                        hr.cut_right(22.0),
                        icons::REPEAT,
                        false,
                        None,
                    );
                    ui.tooltip(&b, "Look for newly installed plugins");
                    if b.clicked {
                        s.rescan_plugins();
                    }
                }
                if h != "Plugins" {
                    let ir = hr.cut_left(16.0);
                    ui.icon(ir, category_icon(h), 12.0, category_color(h));
                    hr.cut_left(6.0);
                }
                ui.text(hr, h, t.caption, t.text_dim, Align::Start);
                ui.describe_label(W::new(("browser-heading", h)), hr, h);
            }
            Row::Device(d, open) => {
                let id = W::new(("browser-dev", d.type_id));
                let mut cells = row.shrink2(4.0, 0.0);
                let has_presets = !peel_devices::presets(d.type_id).is_empty();
                let caret_r = cells.cut_left(16.0);
                // An instrument row ends in its add button.
                let add_r = instrument.then(|| cells.cut_right(ADD_W));
                let resp = ui.interact_button(
                    id,
                    Rect {
                        x: caret_r.right(),
                        w: cells.right() - caret_r.right(),
                        ..row
                    },
                    d.name,
                );
                let add = add_r.map(|r| add_button(ui, id.with("add"), r, d.name));
                let picked = instrument && ui.state::<Picked>(picked_id).row == Some(id);
                let over = resp.hovered || add.as_ref().is_some_and(|a| a.hovered);
                let hot = ui
                    .animate_bool(id, over)
                    .max(if picked { 0.7 } else { 0.0 });
                if hot > 0.0 {
                    ui.scene.rect(row, t.radius_m, t.raised.alpha(hot));
                }
                if has_presets {
                    let caret = ui.interact_node(
                        id.with("caret"),
                        caret_r,
                        peel_ui::FocusKind::Activate,
                        peel_ui::Semantics::new(peel_ui::Role::DisclosureTriangle)
                            .label(format!("{} presets", d.name))
                            .expanded(open),
                    );
                    let c = if caret.hovered {
                        t.text
                    } else if open || hot > 0.0 {
                        t.text_dim
                    } else {
                        t.text_faint.alpha(0.7)
                    };
                    ui.icon(
                        caret_r,
                        if open {
                            icons::CARET_DOWN
                        } else {
                            icons::CARET_RIGHT
                        },
                        10.0,
                        c,
                    );
                    ui.focus_ring(id.with("caret"), caret_r, 4.0);
                    ui.tooltip(&caret, "Presets");
                    if caret.clicked {
                        let set = &mut ui.state::<Expanded>(expanded_id).0;
                        if !set.remove(d.type_id) {
                            set.insert(d.type_id);
                        }
                    }
                }
                let cc = category_color(d.category);
                cells.cut_left(4.0);
                ui.text(
                    cells,
                    d.name,
                    t.body,
                    t.text.mix(t.text_dim, 0.15 * (1.0 - hot)),
                    Align::Start,
                );
                if hot > 0.2 {
                    let lit = add.as_ref().is_some_and(|a| a.hovered);
                    ui.icon(
                        add_r.unwrap_or_else(|| cells.cut_right(18.0)),
                        icons::PLUS,
                        13.0,
                        if lit { t.text } else { t.text_dim.alpha(hot) },
                    );
                }
                if instrument {
                    ui.tooltip(
                        &resp,
                        &format!(
                            "{}. Click to hear it, double-click to add it",
                            d.description
                        ),
                    );
                } else {
                    ui.tooltip(&resp, d.description);
                }
                ui.focus_ring(id, row, t.radius_m);
                if let Some(r) = add_r {
                    ui.focus_ring(id.with("add"), r, t.radius_s);
                }
                if resp.hovered {
                    ui.set_cursor(CursorIcon::Grab);
                }
                if resp.dragging && ui.drag_distance() > 6.0 && ui.dragging_payload().is_none() {
                    ui.start_drag(DragPayload {
                        data: Box::new(BrowserItem::Device(d.type_id)),
                        label: d.name.to_string(),
                        color: cc,
                    });
                }
                let add_clicked = add.is_some_and(|a| a.clicked);
                if !instrument {
                    if resp.clicked {
                        add_device(s, ws, d, None);
                    }
                } else {
                    match pick(ui, picked_id, id, &resp, add_clicked) {
                        Pick::Add => add_device(s, ws, d, None),
                        Pick::Hear => {
                            s.preview_device(d.type_id, rack::default_preset(d.type_id));
                        }
                        Pick::Nothing => {}
                    }
                }
            }
            Row::Scanning => {
                let mut cells = row.shrink2(6.0, 0.0);
                cells.cut_top(6.0);
                ui.text(
                    cells,
                    "Looking for plugins…",
                    t.caption,
                    t.text_faint,
                    Align::Start,
                );
            }
            Row::NoPlugins => {
                let mut cells = row.shrink2(6.0, 0.0);
                cells.cut_top(6.0);
                let label = "Rescan";
                let lw = ui.measure(label, t.caption).x + 12.0;
                let br = cells.cut_right(lw);
                let b = ui.interact_button(W::new("browser-rescan"), br, "Rescan for plugins");
                let c = if b.hovered { t.text } else { t.text_dim };
                ui.text(br, label, t.caption, c, Align::Center);
                if b.hovered {
                    ui.set_cursor(CursorIcon::Pointer);
                }
                ui.focus_ring(b.id, br, t.radius_s);
                ui.tooltip(&b, "Look for newly installed plugins");
                if b.clicked {
                    s.rescan_plugins();
                }
                ui.text(
                    cells,
                    "No plugins found",
                    t.caption,
                    t.text_faint,
                    Align::Start,
                );
            }
            Row::Plugin(i) => {
                let p = &plugins[i];
                let id = W::new(("browser-plugin", p.type_id.as_str()));
                let mut cells = row.shrink2(4.0, 0.0);
                cells.cut_left(16.0);
                // A plugin cannot be heard before it is loaded, so a click
                // on an instrument only picks it; adding works as for the
                // built-in instruments.
                let add_r = instrument.then(|| cells.cut_right(ADD_W));
                let resp = ui.interact_button(
                    id,
                    Rect {
                        w: cells.right() - row.x,
                        ..row
                    },
                    &p.name,
                );
                let add = add_r.map(|r| add_button(ui, id.with("add"), r, &p.name));
                let picked = instrument && ui.state::<Picked>(picked_id).row == Some(id);
                let over = resp.hovered || add.as_ref().is_some_and(|a| a.hovered);
                let hot = ui
                    .animate_bool(id, over)
                    .max(if picked { 0.7 } else { 0.0 });
                if hot > 0.0 {
                    ui.scene.rect(row, t.radius_m, t.raised.alpha(hot));
                }
                let cc = t.text_dim;
                cells.cut_left(4.0);
                if hot > 0.2 {
                    let lit = add.as_ref().is_some_and(|a| a.hovered);
                    ui.icon(
                        add_r.unwrap_or_else(|| cells.cut_right(18.0)),
                        icons::PLUS,
                        13.0,
                        if lit { t.text } else { t.text_dim.alpha(hot) },
                    );
                }
                if instrument {
                    ui.tooltip(&resp, "Double-click to add it");
                }
                if let Some(r) = add_r {
                    ui.focus_ring(id.with("add"), r, t.radius_s);
                }
                let vendor_w = ui.measure(&p.vendor, t.caption).x.min(cells.w * 0.4);
                let vendor_r = cells.cut_right(vendor_w);
                ui.text(vendor_r, &p.vendor, t.caption, t.text_faint, Align::End);
                cells.cut_right(6.0);
                let shown = ui.fit_text(&p.name, t.label, cells.w);
                ui.text(cells, &shown, t.body, t.text, Align::Start);
                ui.focus_ring(id, row, t.radius_m);
                if resp.hovered {
                    ui.set_cursor(CursorIcon::Grab);
                }
                if resp.dragging && ui.drag_distance() > 6.0 && ui.dragging_payload().is_none() {
                    ui.start_drag(DragPayload {
                        data: Box::new(BrowserItem::Plugin {
                            type_id: p.type_id.clone(),
                            name: p.name.clone(),
                            instrument,
                        }),
                        label: p.name.clone(),
                        color: cc,
                    });
                }
                let add_clicked = add.is_some_and(|a| a.clicked);
                if !instrument {
                    if resp.clicked {
                        add_plugin(s, ws, &p.type_id, &p.name, instrument);
                    }
                } else {
                    match pick(ui, picked_id, id, &resp, add_clicked) {
                        Pick::Add => add_plugin(s, ws, &p.type_id, &p.name, instrument),
                        Pick::Hear => s.stop_preview(),
                        Pick::Nothing => {}
                    }
                }
            }
            Row::Preset(d, preset) => {
                let id = W::new(("browser-preset", d.type_id, preset));
                let mut cells = row.shrink2(4.0, 0.0);
                let add_r = instrument.then(|| cells.cut_right(ADD_W));
                let resp = ui.interact_button(
                    id,
                    Rect {
                        w: cells.right() - row.x,
                        ..row
                    },
                    preset,
                );
                let add = add_r.map(|r| add_button(ui, id.with("add"), r, preset));
                let picked = instrument && ui.state::<Picked>(picked_id).row == Some(id);
                let over = resp.hovered || add.as_ref().is_some_and(|a| a.hovered);
                let hot = ui
                    .animate_bool(id, over)
                    .max(if picked { 0.85 } else { 0.0 });
                if hot > 0.0 {
                    ui.scene.rect(row, t.radius_m, t.raised.alpha(hot * 0.8));
                }
                if let Some(r) = add_r
                    && hot > 0.2
                {
                    let lit = add.as_ref().is_some_and(|a| a.hovered);
                    ui.icon(
                        r,
                        icons::PLUS,
                        13.0,
                        if lit { t.text } else { t.text_dim.alpha(hot) },
                    );
                    ui.focus_ring(id.with("add"), r, t.radius_s);
                }
                if instrument {
                    ui.tooltip(&resp, "Click to hear it, double-click to add it");
                }
                cells.cut_left(16.0 + 14.0);
                let cc = category_color(d.category);
                ui.scene.vline(
                    cells.x - 8.0,
                    row.y + 4.0,
                    row.bottom() - 4.0,
                    t.line_strong,
                );
                ui.text(
                    cells,
                    preset,
                    t.body,
                    t.text_dim.mix(t.text, hot),
                    Align::Start,
                );
                if resp.hovered {
                    ui.set_cursor(CursorIcon::Grab);
                }
                if resp.dragging && ui.drag_distance() > 6.0 && ui.dragging_payload().is_none() {
                    ui.start_drag(DragPayload {
                        data: Box::new(BrowserItem::Preset(d.type_id, preset)),
                        label: preset.to_string(),
                        color: cc,
                    });
                }
                ui.focus_ring(id, row, t.radius_m);
                let add_clicked = add.is_some_and(|a| a.clicked);
                if !instrument {
                    if resp.clicked {
                        add_device(s, ws, d, Some(preset));
                    }
                } else {
                    match pick(ui, picked_id, id, &resp, add_clicked) {
                        Pick::Add => add_device(s, ws, d, Some(preset)),
                        Pick::Hear => s.preview_device(d.type_id, Some(preset)),
                        Pick::Nothing => {}
                    }
                }
            }
        }
    }
    ui.scene.pop_clip();
}

fn add_device(
    s: &mut Session,
    ws: &mut Workspace,
    d: &'static peel_dsp::DeviceDescriptor,
    preset: Option<&str>,
) {
    match d.kind {
        DeviceKind::Instrument => {
            if let Some(id) = rack::add_instrument_preset(s, d.type_id, preset) {
                ws.selected_channel = Some(id);
                ws.focus = Some(Focus::Channel(id));
                ws.dock = DockTab::Rack;
            }
        }
        DeviceKind::Effect => {
            // Insert on the focused mixer track, or the selected channel's track.
            let track = match ws.focus {
                Some(Focus::Track(t)) => Some(t),
                _ => ws
                    .selected_channel
                    .and_then(|c| s.project().channel(c))
                    .map(|c| c.track),
            };
            if let Some(track) = track {
                crate::mixer::add_insert_preset(s, track, d.type_id, preset);
                ws.focus = Some(Focus::Track(track));
            }
        }
    }
}

fn add_plugin(s: &mut Session, ws: &mut Workspace, type_id: &str, name: &str, instrument: bool) {
    if instrument {
        if let Some(id) = rack::add_plugin_channel(s, type_id, name) {
            ws.selected_channel = Some(id);
            ws.focus = Some(Focus::Channel(id));
            ws.dock = DockTab::Rack;
        }
    } else {
        let track = match ws.focus {
            Some(Focus::Track(t)) => Some(t),
            _ => ws
                .selected_channel
                .and_then(|c| s.project().channel(c))
                .map(|c| c.track),
        };
        if let Some(track) = track {
            crate::mixer::add_plugin_insert(s, track, type_id, name);
            ws.focus = Some(Focus::Track(track));
        }
    }
}

fn read_dir(dir: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let hidden = p
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'));
            if hidden {
                continue;
            }
            if p.is_dir() {
                dirs.push(p);
            } else if peel_media::is_audio_file(&p) {
                files.push(p);
            }
        }
    }
    dirs.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    files.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    (dirs, files)
}

fn samples(ui: &mut Ui, r: Rect, query: &str, s: &mut Session, ws: &mut Workspace) {
    let t = ui.theme.clone();
    let mut body = r;
    let nav = body.cut_top(28.0);
    body.cut_top(4.0);
    let Some(dir) = ws.sample_dir.clone() else {
        return;
    };
    let cache_id = W::new("browser-dir-cache");
    if ui.state::<DirCache>(cache_id).dir.as_ref() != Some(&dir) {
        let (dirs, files) = read_dir(&dir);
        let c = ui.state::<DirCache>(cache_id);
        c.dir = Some(dir.clone());
        c.dirs = dirs;
        c.files = files;
    }
    let mut nav_row = nav;
    let up = ui.icon_button(
        W::new("browser-up"),
        nav_row.cut_left(28.0),
        icons::CARET_LEFT,
        false,
        None,
    );
    ui.tooltip(&up, "Parent folder");
    if up.clicked
        && let Some(parent) = dir.parent()
    {
        ws.sample_dir = Some(parent.to_path_buf());
        ws.browser_scroll = 0.0;
    }
    let pick = ui.icon_button(
        W::new("browser-pick"),
        nav_row.cut_right(28.0),
        icons::FOLDER_OPEN,
        false,
        None,
    );
    ui.tooltip(&pick, "Choose a folder");
    if pick.clicked
        && let Some(p) = crate::dialogs::folder(&dir)
    {
        ws.sample_dir = Some(p);
        ws.browser_scroll = 0.0;
    }
    nav_row.cut_left(4.0);
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.display().to_string());
    let name = ui.fit_text(&name, t.label, nav_row.w);
    ui.text(nav_row, &name, t.label, t.text_dim, Align::Start);

    let (dirs, files) = {
        let c = ui.state::<DirCache>(cache_id);
        (c.dirs.clone(), c.files.clone())
    };
    let matches = |p: &PathBuf| {
        query.is_empty()
            || p.file_name()
                .is_some_and(|n| n.to_string_lossy().to_lowercase().contains(query))
    };
    let dirs: Vec<PathBuf> = dirs.into_iter().filter(matches).collect();
    let files: Vec<PathBuf> = files.into_iter().filter(matches).collect();
    let total = dirs.len() + files.len();
    let mut off = vec2(0.0, ws.browser_scroll);
    ui.scroll_area(
        W::new("browser-scroll"),
        body,
        vec2(body.w, total as f32 * ROW_H + 8.0),
        &mut off,
    );
    ws.browser_scroll = off.y;
    if total == 0 {
        ui.text(
            body.with_h(48.0),
            "No audio files here",
            t.body,
            t.text_faint,
            Align::Center,
        );
    }
    ui.scene.push_clip(body);
    let mut y = body.y - off.y;
    for (i, p) in dirs.iter().chain(files.iter()).enumerate() {
        let row = Rect {
            x: body.x,
            y,
            w: body.w - 8.0,
            h: ROW_H,
        };
        y += ROW_H;
        if row.bottom() < body.y || row.y > body.bottom() {
            continue;
        }
        let is_dir = i < dirs.len();
        let id = W::new(("browser-file", p));
        let resp = ui.interact(id, row);
        let hot = ui.animate_bool(id, resp.hovered);
        if hot > 0.0 {
            ui.scene.rect(row, t.radius_m, t.raised.alpha(hot));
        }
        let mut cells = row.shrink2(8.0, 0.0);
        let icon = if is_dir {
            icons::FOLDER
        } else {
            icons::WAVEFORM
        };
        ui.icon(
            cells.cut_left(18.0),
            icon,
            14.0,
            if is_dir { t.text_dim } else { t.accent },
        );
        cells.cut_left(8.0);
        let fname = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let fname = ui.fit_text(&fname, t.body, cells.w);
        ui.text(cells, &fname, t.body, t.text, Align::Start);
        if resp.hovered {
            ui.set_cursor(if is_dir {
                CursorIcon::Pointer
            } else {
                CursorIcon::Grab
            });
        }
        if is_dir {
            if resp.clicked {
                ws.sample_dir = Some(p.clone());
                ws.browser_scroll = 0.0;
            }
        } else {
            if resp.pressed {
                s.preview_file(p);
            }
            if resp.dragging && ui.drag_distance() > 6.0 && ui.dragging_payload().is_none() {
                let label = p
                    .file_stem()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                ui.start_drag(DragPayload {
                    data: Box::new(BrowserItem::Sample(p.clone())),
                    label,
                    color: t.accent,
                });
            }
            ui.tooltip(&resp, "Click to listen, drag onto the Channel Rack");
        }
    }
    ui.scene.pop_clip();
}

fn patterns(ui: &mut Ui, r: Rect, query: &str, s: &mut Session, ws: &mut Workspace) {
    let t = ui.theme.clone();
    let list: Vec<(Id, String, peel_document::Color, usize, f64)> = s
        .project()
        .patterns
        .iter()
        .filter(|p| query.is_empty() || p.name.to_lowercase().contains(query))
        .map(|p| {
            (
                p.id,
                p.name.clone(),
                p.color,
                s.project().pattern_usage(p.id),
                p.length.as_f64(),
            )
        })
        .collect();
    let selected = s.selected_pattern();
    let row_h = 34.0;
    let mut off = vec2(0.0, ws.browser_scroll);
    ui.scroll_area(
        W::new("browser-scroll"),
        r,
        vec2(r.w, list.len() as f32 * row_h + 8.0),
        &mut off,
    );
    ws.browser_scroll = off.y;
    ui.scene.push_clip(r);
    let mut y = r.y - off.y;
    for (pid, name, color, uses, len) in list {
        let row = Rect {
            x: r.x,
            y,
            w: r.w - 8.0,
            h: row_h - 4.0,
        };
        y += row_h;
        let id = W::new(("browser-pattern", pid));
        let resp = ui.interact(id, row);
        let hot = ui.animate_bool(id, resp.hovered);
        let on = pid == selected;
        ui.scene.rect(
            row,
            t.radius_m,
            if on {
                t.raised
            } else {
                t.raised.alpha(hot * 0.7)
            },
        );
        let c = col(color);
        let mut cells = row.shrink2(8.0, 0.0);
        let sw = cells.cut_left(14.0);
        ui.scene
            .rect(Rect::from_center(sw.center(), 14.0, 14.0), 4.0, c);
        cells.cut_left(10.0);
        let info = format!(
            "{}  ×{}",
            crate::util::format_length(peel_document::Time::from_f64(len, 4)),
            uses
        );
        let info_w = ui.measure(&info, t.caption).x;
        let info_r = cells.cut_right(info_w + 2.0);
        ui.text(info_r, &info, t.caption, t.text_faint, Align::End);
        let name = ui.fit_text(&name, t.label, cells.w - 6.0);
        ui.text(cells, &name, t.label, t.text, Align::Start);
        if resp.pressed {
            s.select_pattern(pid);
        }
        if resp.dragging && ui.drag_distance() > 6.0 && ui.dragging_payload().is_none() {
            ui.start_drag(DragPayload {
                data: Box::new(BrowserItem::Pattern(pid)),
                label: name.clone(),
                color: c,
            });
        }
        if resp.hovered {
            ui.set_cursor(CursorIcon::Grab);
        }
        ui.tooltip(&resp, "Drag into the playlist");
    }
    ui.scene.pop_clip();
}
