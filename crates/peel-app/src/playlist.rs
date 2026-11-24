//! Playlist: the arrangement. Pattern clips are linked instances; editing
//! the pattern changes every instance.

use std::collections::{HashMap, HashSet};

use peel_document::{Clip, ClipContent, Id, Pattern, Time, Track};
use peel_session::{PlayMode, Session};
use peel_ui::Id as W;
use peel_ui::{Align, Color, CursorIcon, MenuItem, Rect, Ui, icons, vec2};

use crate::browser::BrowserItem;
use crate::studio::current_pattern;
use crate::util::{SCROLLBAR, col, format_length, ink_on};
use crate::workspace::{DockTab, Focus, Tool, Workspace};

const HEAD_W: f32 = 150.0;
const RULER_H: f32 = 26.0;
/// Extra ruler height for the marker row when the arrangement has markers.
const MARKER_H: f32 = 20.0;
const TEMPO_H: f32 = 64.0;
const EDGE: f32 = 8.0;

#[derive(Clone, Copy, PartialEq, Default)]
enum Drag {
    #[default]
    None,
    Move,
    Resize,
    Paint,
    Erase,
    Seek,
    Marquee,
    TrimStart,
    FadeIn,
    FadeOut,
    AutoPoint,
    /// Bending the segment after `auto_point`.
    AutoTension,
}

/// Which part of a clip the pointer is over.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Part {
    Body,
    Left,
    Right,
    FadeIn,
    FadeOut,
    /// The envelope area of an automation clip.
    AutoBody,
}

#[derive(Clone, Default)]
struct DragState {
    /// Breakpoint being dragged in an automation clip.
    auto_point: Option<usize>,
    /// Pointer height and tension when a tension handle was grabbed.
    tension_from: (f32, f32),
    kind: Drag,
    clip: Option<Clip>,
    /// Every clip being moved, as it was when the drag started.
    origin: Vec<Clip>,
    start_lane: i32,
    marquee_from: (f64, f32),
    start_beat: f64,
    gesture: u64,
    last_paint: Option<(Id, Time)>,
}

#[derive(Clone, Copy)]
enum ClipAction {
    None,
    Open,
    Unique,
    Rename,
    Mute,
    Duplicate,
    Copy,
    Cut,
    Delete,
    Reverse,
    Render,
    Chop,
}

struct View {
    lanes: Rect,
    zoom_x: f32,
    scroll: f32,
    lane_h: f32,
    scroll_y: f32,
}

impl View {
    fn x(&self, beat: f64) -> f32 {
        self.lanes.x + (beat as f32 - self.scroll) * self.zoom_x
    }

    fn beat(&self, x: f32) -> f64 {
        ((x - self.lanes.x) / self.zoom_x + self.scroll) as f64
    }

    fn lane_y(&self, i: usize) -> f32 {
        self.lanes.y + i as f32 * self.lane_h - self.scroll_y
    }

    fn lane_at(&self, y: f32) -> i32 {
        ((y - self.lanes.y + self.scroll_y) / self.lane_h).floor() as i32
    }
}

pub fn draw(ui: &mut Ui, r: Rect, s: &mut Session, ws: &mut Workspace) {
    let t = ui.theme.clone();
    ui.scene.rect(r, 0.0, t.panel);
    let mut body = r;
    let head = body.cut_top(ui.theme.header_h);
    header(ui, head, s, ws);
    ui.scene.hline(r.x, r.right(), head.bottom() - 0.5, t.line);
    let hbar_row = body.cut_bottom(SCROLLBAR);
    let vbar_col = body.cut_right(SCROLLBAR);

    let arr = s.project().arrangement().clone();
    let ruler_h = if arr.markers.is_empty() {
        RULER_H
    } else {
        RULER_H + MARKER_H
    };
    let mut top = body.cut_top(ruler_h);
    let tempo_r = ws.playlist.show_tempo.then(|| body.cut_top(TEMPO_H));
    let heads = body.cut_left(HEAD_W);
    top.cut_left(HEAD_W);
    let lanes_r = body;

    // A newly opened project starts with the whole song in view.
    if ws.playlist.fit > 0 {
        ws.playlist.fit -= 1;
        let end = arr.end().as_f64() as f32;
        if end > 0.0 && lanes_r.w > 0.0 {
            ws.playlist.zoom_x = (lanes_r.w / (end + 4.0)).clamp(1.5, 200.0);
            ws.playlist.scroll_beats = 0.0;
            ws.playlist.scroll_y = 0.0;
            let n = arr.tracks.len().max(1) as f32;
            ws.playlist.lane_h = (lanes_r.h / n).clamp(30.0, 52.0);
        }
    }

    // Navigation.
    let nav = Rect {
        x: heads.x,
        y: top.y,
        w: lanes_r.right() - heads.x,
        h: lanes_r.bottom() - top.y,
    };
    if ui.pointer_in(nav) {
        let sc = ui.input.scroll;
        let m = ui.input.modifiers;
        if sc != peel_ui::Vec2::ZERO || ui.input.zoom != 0.0 {
            ws.playlist.fit = 0;
            if m.command() || ui.input.zoom != 0.0 {
                let p = ui.pointer().unwrap_or_default();
                let before = (p.x - lanes_r.x) / ws.playlist.zoom_x + ws.playlist.scroll_beats;
                let f = if ui.input.zoom != 0.0 {
                    1.0 + ui.input.zoom
                } else {
                    (1.0f32 + sc.y * 0.004).clamp(0.5, 2.0)
                };
                ws.playlist.zoom_x = (ws.playlist.zoom_x * f).clamp(1.5, 200.0);
                ws.playlist.scroll_beats = before - (p.x - lanes_r.x) / ws.playlist.zoom_x;
            } else if m.alt {
                ws.playlist.lane_h = (ws.playlist.lane_h * (1.0 + sc.y * 0.004)).clamp(28.0, 140.0);
            } else {
                let (dx, dy) = if m.shift && sc.x == 0.0 {
                    (sc.y, 0.0)
                } else {
                    (sc.x, sc.y)
                };
                ws.playlist.scroll_beats -= dx / ws.playlist.zoom_x;
                ws.playlist.scroll_y -= dy;
            }
            ui.input.scroll = peel_ui::Vec2::ZERO;
            ui.input.zoom = 0.0;
        }
    }
    ws.playlist.scroll_beats = ws.playlist.scroll_beats.max(0.0);
    let content_h = (arr.tracks.len() as f32 + 1.0) * ws.playlist.lane_h;
    ws.playlist.scroll_y = ws
        .playlist
        .scroll_y
        .clamp(0.0, (content_h - lanes_r.h).max(0.0));
    // Scrollbars: the timeline extends 16 bars past the last clip.
    let view_beats = lanes_r.w / ws.playlist.zoom_x;
    let song_beats = arr.end().as_f64() as f32 + 64.0;
    let content_beats = song_beats.max(ws.playlist.scroll_beats + view_beats);
    let hbar = Rect {
        x: lanes_r.x,
        w: lanes_r.w,
        ..hbar_row
    };
    let scrolled_h = ui.scrollbar(
        W::new("pl-hbar"),
        hbar,
        true,
        &mut ws.playlist.scroll_beats,
        view_beats,
        content_beats,
    );
    let vbar = Rect {
        y: lanes_r.y,
        h: lanes_r.h,
        ..vbar_col
    };
    let scrolled_v = ui.scrollbar(
        W::new("pl-vbar"),
        vbar,
        false,
        &mut ws.playlist.scroll_y,
        lanes_r.h,
        content_h,
    );
    if scrolled_h || scrolled_v {
        ws.playlist.fit = 0;
    }
    let v = View {
        lanes: lanes_r,
        zoom_x: ws.playlist.zoom_x,
        scroll: ws.playlist.scroll_beats,
        lane_h: ws.playlist.lane_h,
        scroll_y: ws.playlist.scroll_y,
    };

    lanes_bg(ui, &v, &arr.tracks, s);
    ruler(ui, top, &v, s);
    markers(ui, top, &v, s);
    if let Some(mut tr) = tempo_r {
        let head = tr.cut_left(HEAD_W);
        tempo_lane(ui, head, tr, &v, s);
    }
    track_heads(ui, heads, &v, &arr.tracks, s);
    clips(ui, &v, &arr.tracks, &arr.clips, s, ws);
    take_preview(ui, &v, &arr.tracks, s);

    // Patterns and samples dragged from the browser, and files from the OS.
    let accepts = |i: &BrowserItem| matches!(i, BrowserItem::Pattern(_) | BrowserItem::Sample(_));
    if ui.drop_hover::<BrowserItem>(lanes_r).is_some_and(accepts)
        && let Some((item, p)) = ui.take_drop::<BrowserItem>(lanes_r)
    {
        let lane = v.lane_at(p.y);
        let beat = Time::from_f64(v.beat(p.x).max(0.0), 960).floor_to(ws.playlist.snap);
        if let Some(track) = arr.tracks.get(lane.max(0) as usize).map(|t| t.id) {
            match item {
                BrowserItem::Pattern(pid) => place_clip(s, track, beat, pid),
                BrowserItem::Sample(path) => {
                    if let Err(e) = s.add_audio_clip(track, beat, &path) {
                        s.notify(format!("Could not add {}: {e}", path.display()), true);
                    }
                }
                _ => {}
            }
        }
    }
    if let Some(p) = ui.pointer().filter(|p| lanes_r.contains(*p)) {
        let files = ui.take_dropped_files(lanes_r);
        let lane = v.lane_at(p.y).max(0) as usize;
        let beat = Time::from_f64(v.beat(p.x).max(0.0), 960).floor_to(ws.playlist.snap);
        for (i, path) in files
            .iter()
            .filter(|f| peel_media::is_audio_file(f))
            .enumerate()
        {
            if let Some(track) = arr.tracks.get(lane + i).map(|t| t.id)
                && let Err(e) = s.add_audio_clip(track, beat, path)
            {
                s.notify(format!("Could not add {}: {e}", path.display()), true);
            }
        }
    }

    if arr.clips.is_empty() {
        let hint = Rect {
            x: lanes_r.x,
            y: lanes_r.y + 4.0,
            w: lanes_r.w,
            h: ws.playlist.lane_h * 2.0,
        };
        ui.text(
            hint,
            "Click a lane to paint the selected pattern",
            t.body,
            t.text_faint,
            Align::Center,
        );
    }

    // Playhead.
    if s.mode() == PlayMode::Song {
        let x = v.x(s.position());
        if x >= lanes_r.x && x <= lanes_r.right() {
            let full = Rect {
                y: top.y,
                h: lanes_r.bottom() - top.y,
                ..lanes_r
            };
            let c = if s.is_playing() {
                t.accent
            } else {
                t.accent.alpha(0.55)
            };
            ui.with_layer(peel_ui::LayerId::ABOVE, |ui| {
                ui.scene.push_clip(full);
                ui.scene.rect(
                    Rect {
                        x: x - 1.0,
                        y: top.y,
                        w: 2.0,
                        h: full.h,
                    },
                    1.0,
                    c,
                );
                ui.scene.triangle(
                    Rect {
                        x: x - 7.0,
                        y: top.y + RULER_H - 10.0,
                        w: 14.0,
                        h: 10.0,
                    },
                    peel_ui::Dir::Down,
                    0.5,
                    c,
                );
                ui.scene.pop_clip();
            });
        }
        // Follow playback: page on near the right edge, and come back
        // when the transport wraps to a loop start that is out of view.
        let pos = s.position();
        let last = std::mem::replace(ui.state::<f64>(W::new("pl-follow-last")), pos);
        let out = x < lanes_r.x || x > lanes_r.right();
        let jumped = if s.is_playing() {
            pos + 1e-6 < last
        } else {
            (pos - last).abs() > 1e-6
        };
        let edge = s.is_playing() && x > lanes_r.right() - 40.0 && x <= lanes_r.right() + 40.0;
        if edge || (jumped && out) {
            ws.playlist.scroll_beats = (pos as f32 - 4.0).max(0.0);
        }
    }
}

fn header(ui: &mut Ui, r: Rect, s: &mut Session, ws: &mut Workspace) {
    let t = ui.theme.clone();
    let mut row = r.shrink2(8.0, 4.0);
    // Arrangement picker (the playlist's title).
    let arr_name = s.project().arrangement().name.clone();
    let title_w = ui.measure(&arr_name, t.label).x.min(150.0) + 58.0;
    let title = row.cut_left(title_w);
    row.cut_left(8.0);
    let tresp = ui.interact_button(W::new("arrangement-picker"), title, &arr_name);
    if tresp.hovered {
        ui.scene.rect(title, t.radius_m, t.control_hover);
        ui.set_cursor(CursorIcon::Pointer);
    }
    let mut tr = title.shrink2(8.0, 0.0);
    ui.icon(tr.cut_left(18.0), icons::COLUMNS, 15.0, t.accent);
    tr.cut_left(6.0);
    ui.icon(tr.cut_right(12.0), icons::CARET_DOWN, 11.0, t.text_dim);
    let shown = ui.fit_text(&arr_name, t.label, tr.w);
    ui.text(tr, &shown, t.label, t.text, Align::Start);
    ui.tooltip(&tresp, "Arrangements");
    ui.focus_ring(tresp.id, title, t.radius_m);
    if tresp.clicked {
        ui.open_popup(W::new("arrangement-menu"), title);
    }
    arrangement_menu(ui, title, s);
    // Editing tools in one well, then the tempo lane and grid.
    let tools = [
        (Tool::Draw, icons::PENCIL, "Draw"),
        (Tool::Paint, icons::BRUSH, "Paint"),
        (Tool::Select, icons::CURSOR, "Select"),
        (Tool::Slice, icons::KNIFE, "Slice"),
        (Tool::Erase, icons::ERASER, "Erase"),
    ];
    let well = row.cut_left(tools.len() as f32 * 30.0 + 4.0);
    ui.well(well);
    let mut cells = well.shrink(2.0);
    for (tool, icon, tip) in tools {
        let b = ui.tool_button(
            W::new(("pl-tool", tip)),
            cells.cut_left(30.0),
            icon,
            tip,
            ws.playlist.tool == tool,
        );
        if b.clicked {
            ws.playlist.tool = tool;
        }
    }
    row.cut_left(8.0);
    ui.divider_v(row.cut_left(1.0));
    row.cut_left(8.0);
    let tb = ui.icon_button(
        W::new("pl-tempo"),
        row.cut_left(30.0),
        icons::METRONOME,
        ws.playlist.show_tempo,
        None,
    );
    ui.name(&tb, "Tempo lane");
    ui.tooltip(&tb, "Tempo changes");
    if tb.clicked {
        ws.playlist.show_tempo = !ws.playlist.show_tempo;
    }
    row.cut_left(4.0);
    let snap_r = row.cut_left(64.0);
    let sresp = ui.value_button(
        W::new("pl-snap"),
        snap_r,
        icons::MAGNET,
        &format_length(ws.playlist.snap),
        "Grid",
        false,
    );
    if sresp.clicked {
        ui.open_popup(W::new("pl-snap-menu"), snap_r);
    }
    let snaps = [
        Time::beats(4),
        Time::beats(1),
        Time::new(1, 2),
        Time::new(1, 4),
    ];
    let items: Vec<MenuItem> = snaps
        .iter()
        .map(|x| MenuItem::new(format_length(*x)).checked(*x == ws.playlist.snap))
        .collect();
    if let Some(i) = ui.menu(
        W::new("pl-snap-menu"),
        vec2(snap_r.x, snap_r.bottom() + 4.0),
        &items,
    ) {
        ws.playlist.snap = snaps[i];
    }
    // Selected pattern chip (what painting places).
    let pid = current_pattern(s);
    if let Some(p) = s.project().pattern(pid).cloned() {
        // Quiet: a brush in the pattern's color and its name.
        let w = ui.measure(&p.name, t.label).x + 34.0;
        let chip = row.cut_right(w.min(200.0).min(row.w.max(0.0)));
        let resp = ui.interact(W::new("pl-paint-pattern"), chip);
        let mut cr = chip.shrink2(6.0, 0.0);
        ui.icon(cr.cut_left(16.0), icons::BRUSH, 13.0, col(p.color));
        cr.cut_left(6.0);
        let name = ui.fit_text(&p.name, t.label, cr.w);
        ui.text(cr, &name, t.label, t.text_dim, Align::Start);
        ui.tooltip(&resp, "Pattern the Paint tool places");
    }
}

fn lanes_bg(ui: &mut Ui, v: &View, tracks: &[Track], s: &Session) {
    let t = ui.theme.clone();
    let c = v.lanes;
    ui.scene.push_clip(c);
    ui.scene.rect(c, 0.0, t.panel_alt);
    for (i, _) in tracks.iter().enumerate() {
        let y = v.lane_y(i);
        if i % 2 == 1 {
            ui.scene.rect(
                Rect {
                    x: c.x,
                    y,
                    w: c.w,
                    h: v.lane_h,
                },
                0.0,
                Color::WHITE.alpha(0.012),
            );
        }
        ui.scene.hline(c.x, c.right(), y + v.lane_h, t.line);
    }
    let sig = s.project().time_signature;
    let bar = sig.bar_length().as_f64();
    let step = if v.zoom_x >= 14.0 { 1.0 } else { bar };
    let mut b = (v.scroll as f64 / step).floor() * step;
    while v.x(b) <= c.right() {
        let x = v.x(b);
        let is_bar = (b / bar).fract().abs() < 1e-6;
        let bar_idx = (b / bar).round() as i64;
        let color = if is_bar && bar_idx % 4 == 0 {
            t.grid_bar.alpha(1.6)
        } else if is_bar {
            t.grid_bar
        } else {
            t.grid_step
        };
        ui.scene.vline(x, c.y, c.bottom(), color);
        b += step;
    }
    if let Some((a, e)) = s.project().arrangement().loop_range {
        let (x0, x1) = (v.x(a.as_f64()), v.x(e.as_f64()));
        ui.scene.rect(
            Rect {
                x: x0,
                y: c.y,
                w: x1 - x0,
                h: c.h,
            },
            0.0,
            t.accent.alpha(0.03),
        );
    }
    ui.scene.pop_clip();
}

fn ruler(ui: &mut Ui, r: Rect, v: &View, s: &mut Session) {
    let t = ui.theme.clone();
    ui.scene.push_clip(r);
    ui.scene.rect(r, 0.0, t.panel);
    let bar = s.project().time_signature.bar_length().as_f64();
    let every = if v.zoom_x * bar as f32 >= 36.0 {
        1.0
    } else if v.zoom_x * bar as f32 >= 12.0 {
        4.0
    } else {
        8.0
    };
    let mut b = (v.scroll as f64 / (bar * every)).floor() * bar * every;
    while v.x(b) < r.right() {
        let x = v.x(b);
        ui.scene
            .vline(x, r.y + 12.0, r.bottom(), t.text_faint.alpha(0.5));
        ui.text_at(
            vec2(x + 4.0, r.y + 3.0),
            &format!("{}", (b / bar).round() as i64 + 1),
            t.caption,
            t.text_dim,
        );
        b += bar * every;
    }
    if let Some((a, e)) = s.project().arrangement().loop_range {
        let (x0, x1) = (v.x(a.as_f64()), v.x(e.as_f64()));
        ui.scene.rect(
            Rect {
                x: x0,
                y: r.bottom() - 4.0,
                w: x1 - x0,
                h: 4.0,
            },
            2.0,
            t.accent.alpha(0.7),
        );
    }
    ui.scene.pop_clip();
    let id = W::new("pl-ruler");
    let resp = ui.interact(id, r);
    if resp.hovered {
        ui.set_cursor(CursorIcon::Pointer);
    }
    ui.tooltip(&resp, "Click to move the playhead. Drag to set the loop.");
    let st_id = W::new("pl-ruler-drag");
    if resp.pressed
        && let Some(p) = ui.pointer()
    {
        let beat = Time::from_f64(v.beat(p.x).max(0.0), 960).floor_to(Time::beats(1));
        ui.state::<DragState>(st_id).start_beat = beat.as_f64();
        ui.state::<DragState>(st_id).kind = Drag::Seek;
        if s.mode() != PlayMode::Song {
            s.set_mode(PlayMode::Song);
        }
        s.seek(beat.as_f64());
    }
    if resp.dragging
        && let Some(p) = ui.pointer()
        && ui.state::<DragState>(st_id).kind == Drag::Seek
    {
        let a = ui.state::<DragState>(st_id).start_beat;
        let b = Time::from_f64(v.beat(p.x).max(0.0), 960)
            .round_to(bar_time(s))
            .as_f64();
        if (b - a).abs() >= bar_time(s).as_f64() {
            let (lo, hi) = (a.min(b), a.max(b));
            let lo_t = Time::from_f64(lo, 960).floor_to(bar_time(s));
            let hi_t = Time::from_f64(hi, 960).round_to(bar_time(s));
            if hi_t > lo_t {
                s.doc.edit_gesture(id.0, "Set loop", |p| {
                    p.arrangement_mut().loop_range = Some((lo_t, hi_t))
                });
            }
        }
    }
    if resp.released {
        s.doc.end_gesture();
    }
    if resp.double_clicked {
        s.doc
            .edit("Clear loop", |p| p.arrangement_mut().loop_range = None);
    }
    // Right-click: markers and loop.
    let menu = id.with("menu");
    if resp.secondary_clicked
        && let Some(p) = ui.pointer()
    {
        let beat = Time::from_f64(v.beat(p.x).max(0.0), 960).round_to(Time::beats(1));
        ui.state::<DragState>(st_id).start_beat = beat.as_f64();
        ui.open_popup(
            menu,
            Rect {
                x: p.x,
                y: p.y,
                w: 0.0,
                h: 0.0,
            },
        );
    }
    let has_loop = s.project().arrangement().loop_range.is_some();
    let items = [
        MenuItem::new("Add marker"),
        MenuItem::new("Clear loop").enabled(has_loop),
    ];
    let at = ui.state::<DragState>(st_id).start_beat;
    if let Some(i) = ui.menu(menu, vec2(r.x, r.bottom()), &items) {
        match i {
            0 => {
                add_marker(s, Time::from_f64(at, 960).round_to(Time::beats(1)));
            }
            _ => s
                .doc
                .edit("Clear loop", |p| p.arrangement_mut().loop_range = None),
        }
    }
}

/// Adds a marker named after its position in the song.
pub fn add_marker(s: &mut Session, at: Time) -> Id {
    let n = s.project().arrangement().markers.len() + 1;
    let m = peel_document::Marker {
        id: Id::new(),
        position: at,
        name: format!("Marker {n}"),
    };
    let id = m.id;
    s.doc.edit("Add marker", |p| {
        let arr = p.arrangement_mut();
        arr.markers.push(m);
        arr.markers.sort_by_key(|m| m.position);
    });
    id
}

/// Space kept between a marker's tag and the next marker's line.
const MARKER_GAP: f32 = 4.0;
/// A tag narrower than this shows only its flag; the name is in the tooltip.
const MARKER_MIN_LABEL: f32 = 28.0;
/// Width of a flag-only tag, which is still a hit target.
const MARKER_FLAG_W: f32 = 9.0;

/// Tag widths for markers at screen `xs` with `natural` label widths, in
/// input order. Each tag ends before the next marker to its right (by
/// position, whatever the input order) and before `right`. Of markers at
/// the same spot, the last one gets the room and the others show only a
/// flag. The bool is whether the tag has room for its name.
pub(crate) fn marker_tags(xs: &[f32], natural: &[f32], right: f32) -> Vec<(f32, bool)> {
    let mut order: Vec<usize> = (0..xs.len()).collect();
    order.sort_by(|&a, &b| xs[a].total_cmp(&xs[b]));
    let mut out = vec![(MARKER_FLAG_W, false); xs.len()];
    for (k, &i) in order.iter().enumerate() {
        let limit = order
            .get(k + 1)
            .map_or(right, |&n| (xs[n] - MARKER_GAP).min(right));
        let w = natural[i].min(limit - xs[i]);
        if w >= MARKER_MIN_LABEL {
            out[i] = (w, true);
        }
    }
    out
}

/// Marker tags in the ruler's lower row: click to jump, drag to move,
/// right-click or double-click to rename, right-click to delete.
fn markers(ui: &mut Ui, r: Rect, v: &View, s: &mut Session) {
    let t = ui.theme.clone();
    let list = s.project().arrangement().markers.clone();
    if list.is_empty() {
        return;
    }
    let row = Rect {
        y: r.y + RULER_H - 4.0,
        h: MARKER_H - 2.0,
        ..r
    };
    let xs: Vec<f32> = list.iter().map(|m| v.x(m.position.as_f64())).collect();
    let natural: Vec<f32> = list
        .iter()
        .map(|m| ui.measure(&m.name, t.caption).x + 14.0)
        .collect();
    let layout = marker_tags(&xs, &natural, r.right());
    ui.scene.push_clip(r);
    let mut rects = Vec::new();
    for (m, (&x, &(w, labelled))) in list.iter().zip(xs.iter().zip(&layout)) {
        let tag = Rect { x, w, ..row };
        rects.push(tag);
        if tag.right() < r.x || tag.x > r.right() {
            continue;
        }
        ui.scene
            .vline(x, r.y + 12.0, row.bottom(), t.text.alpha(0.7));
        ui.scene.rect(tag, t.radius_s, t.control);
        ui.scene.rect(Rect { w: 3.0, ..tag }, 0.0, t.text_dim);
        if labelled {
            // Scrolled partly off the left edge, the name stays readable
            // in the part of the tag still showing.
            let lx = (tag.x + 8.0).max(r.x + 4.0);
            let label = Rect {
                x: lx,
                w: tag.right() - lx - 4.0,
                ..tag
            };
            if label.w > 0.0 {
                let name = ui.fit_text(&m.name, t.caption, label.w);
                ui.with_clip(label, |ui| {
                    ui.text(label, &name, t.caption, t.text, Align::Start)
                });
            }
        }
    }
    ui.scene.pop_clip();
    // Flag-only tags register last, so a flag sharing a spot with a
    // labelled tag stays reachable on top of it.
    let mut order: Vec<usize> = (0..list.len()).collect();
    order.sort_by_key(|&i| !layout[i].1);
    for (m, tag) in order.into_iter().map(|i| (&list[i], rects[i])) {
        let visible = Rect {
            x: tag.x.max(r.x),
            w: (tag.right().min(r.right()) - tag.x.max(r.x)).max(0.0),
            ..tag
        };
        if visible.w <= 0.0 {
            continue;
        }
        let id = W::new(("marker", m.id));
        let resp = ui.interact_button(id, visible, &m.name);
        if resp.hovered || resp.active {
            ui.scene.outline(visible, t.radius_s, 1.0, t.text_dim);
            ui.set_cursor(if resp.dragging {
                CursorIcon::Grabbing
            } else {
                CursorIcon::Pointer
            });
        }
        ui.tooltip(
            &resp,
            &format!("{}  ·  click to jump, drag to move", m.name),
        );
        if resp.dragging
            && ui.drag_distance() > 3.0
            && let Some(p) = ui.pointer()
        {
            let at = Time::from_f64(v.beat(p.x).max(0.0), 960).round_to(Time::beats(1));
            if at != m.position {
                let mid = m.id;
                s.doc.edit_gesture(id.0, "Move marker", |p| {
                    let arr = p.arrangement_mut();
                    if let Some(x) = arr.markers.iter_mut().find(|x| x.id == mid) {
                        x.position = at;
                    }
                    arr.markers.sort_by_key(|m| m.position);
                });
            }
        }
        if resp.released {
            s.doc.end_gesture();
        }
        if resp.clicked && ui.drag_distance() <= 3.0 {
            if s.mode() != PlayMode::Song {
                s.set_mode(PlayMode::Song);
            }
            s.seek(m.position.as_f64());
        }
        let rename = id.with("rename");
        if resp.double_clicked {
            crate::rename::open(ui, rename, visible, &m.name);
        }
        let menu = id.with("menu");
        if resp.secondary_clicked {
            ui.open_popup(menu, visible);
        }
        let items = [MenuItem::new("Rename"), MenuItem::new("Delete")];
        match ui.menu(menu, vec2(visible.x, visible.bottom() + 4.0), &items) {
            Some(0) => crate::rename::open(ui, rename, visible, &m.name),
            Some(_) => {
                let mid = m.id;
                s.doc.edit("Delete marker", |p| {
                    p.arrangement_mut().markers.retain(|x| x.id != mid)
                });
            }
            None => {}
        }
        if let Some(name) = crate::rename::show(ui, rename) {
            let mid = m.id;
            s.doc.edit("Rename marker", |p| {
                if let Some(x) = p.arrangement_mut().markers.iter_mut().find(|x| x.id == mid) {
                    x.name = name;
                }
            });
        }
    }
}

fn bar_time(s: &Session) -> Time {
    s.project().time_signature.bar_length()
}

fn track_heads(ui: &mut Ui, r: Rect, v: &View, tracks: &[Track], s: &mut Session) {
    let t = ui.theme.clone();
    ui.scene.push_clip(r);
    ui.scene
        .vline(r.right() - 0.5, r.y, r.bottom(), t.line_strong);
    for (i, tr) in tracks.iter().enumerate() {
        let y = v.lane_y(i);
        // One continuous header column: lanes are divided by the same
        // separator the canvas uses, not boxed one by one.
        let row = Rect {
            x: r.x,
            y,
            w: r.w - 1.0,
            h: v.lane_h,
        };
        if row.bottom() < r.y || row.y > r.bottom() {
            continue;
        }
        if ui.pointer_in(row) {
            ui.scene.rect(row, 0.0, t.raised.alpha(0.5));
        }
        ui.scene.hline(row.x, r.right(), row.bottom(), t.line);
        let stripe = tr.color.map(col).unwrap_or_else(|| {
            let first = s
                .project()
                .arrangement()
                .clips
                .iter()
                .find(|c| c.track == tr.id)
                .and_then(|c| match c.content {
                    ClipContent::Pattern { pattern } => {
                        s.project().pattern(pattern).map(|p| col(p.color))
                    }
                    _ => None,
                });
            first.unwrap_or(t.text_faint)
        });
        ui.scene.rect(
            Rect {
                w: 3.0,
                ..row.shrink2(0.0, 1.0)
            },
            0.0,
            stripe,
        );
        let mut inner = row.shrink2(10.0, 0.0);
        inner.cut_right(2.0);
        let led_r = inner.cut_right(20.0);
        // Record arm, shown on hover or while armed.
        let arm_r = inner.cut_right(24.0);
        let armed = s.is_lane_armed(tr.id);
        if armed || ui.pointer_in(row) {
            let id = W::new(("pl-arm", tr.id));
            let resp = ui.interact_button(id, arm_r, &format!("Arm {} for recording", tr.name));
            let c = arm_r.center();
            if armed {
                let pulse = if s.is_recording() {
                    0.75 + 0.25 * (ui.time() as f32 * 5.0).sin()
                } else {
                    1.0
                };
                ui.scene.circle(c, 6.5, t.record.alpha(pulse));
            } else {
                let fill = if resp.hovered {
                    t.record.alpha(0.35)
                } else {
                    t.control
                };
                ui.scene.circle_stroke(c, 6.5, fill, 1.2, t.text_faint);
            }
            ui.tooltip(
                &resp,
                if armed {
                    "Disarm"
                } else {
                    "Arm for audio recording"
                },
            );
            if resp.clicked {
                s.arm_lane(tr.id, !armed);
            }
        }
        let led = ui.led(W::new(("pl-led", tr.id)), led_r, !tr.muted, t.play);
        ui.tooltip(&led, if tr.muted { "Unmute lane" } else { "Mute lane" });
        if led.clicked {
            let tid = tr.id;
            s.doc
                .edit(if tr.muted { "Unmute lane" } else { "Mute lane" }, |p| {
                    if let Some(x) = p.arrangement_mut().tracks.iter_mut().find(|x| x.id == tid) {
                        x.muted = !x.muted;
                    }
                });
        }
        let name = ui.fit_text(&tr.name, t.label, inner.w - 4.0);
        ui.text(
            inner,
            &name,
            t.label,
            if tr.muted {
                t.text_faint
            } else {
                t.text.mix(t.text_dim, 0.2)
            },
            Align::Start,
        );
        let name_resp = ui.interact(W::new(("pl-lane-name", tr.id)), inner);
        ui.tooltip(&name_resp, "Double-click to rename");
        if name_resp.double_clicked {
            *ui.state::<Option<Id>>(W::new("pl-rename-lane-target")) = Some(tr.id);
            crate::rename::open(ui, W::new("pl-rename-lane"), row, &tr.name);
        }
        let menu = W::new(("pl-lane-menu", tr.id));
        if name_resp.secondary_clicked {
            ui.open_popup(menu, row);
        }
        let n = tracks.len();
        let items = [
            MenuItem::new("Rename"),
            MenuItem::new("Insert lane above"),
            MenuItem::new("Move up").enabled(i > 0),
            MenuItem::new("Move down").enabled(i + 1 < n),
            MenuItem::Separator,
            MenuItem::new("Delete lane and its clips").enabled(n > 1),
        ];
        let lane_id = tr.id;
        match ui.menu(menu, vec2(row.x, row.bottom() + 4.0), &items) {
            Some(0) => {
                *ui.state::<Option<Id>>(W::new("pl-rename-lane-target")) = Some(lane_id);
                crate::rename::open(ui, W::new("pl-rename-lane"), row, &tr.name);
            }
            Some(1) => s.doc.edit("Insert lane", |p| {
                let arr = p.arrangement_mut();
                arr.tracks.insert(
                    i,
                    Track {
                        id: Id::new(),
                        name: format!("Track {}", n + 1),
                        color: None,
                        muted: false,
                    },
                );
            }),
            Some(2) => s
                .doc
                .edit("Move lane", |p| p.arrangement_mut().tracks.swap(i, i - 1)),
            Some(3) => s
                .doc
                .edit("Move lane", |p| p.arrangement_mut().tracks.swap(i, i + 1)),
            Some(5) => s.doc.edit(format!("Delete {}", tr.name), |p| {
                let arr = p.arrangement_mut();
                arr.tracks.retain(|t| t.id != lane_id);
                arr.clips.retain(|c| c.track != lane_id);
            }),
            _ => {}
        }
    }
    if let Some(name) = crate::rename::show(ui, W::new("pl-rename-lane"))
        && let Some(lane) = *ui.state::<Option<Id>>(W::new("pl-rename-lane-target"))
    {
        s.doc.edit("Rename lane", |p| {
            if let Some(x) = p.arrangement_mut().tracks.iter_mut().find(|x| x.id == lane) {
                x.name = name;
            }
        });
    }
    // Add lane.
    let y = v.lane_y(tracks.len());
    let add = Rect {
        x: r.x,
        y: y + 6.0,
        w: 36.0,
        h: 28.0,
    };
    let b = ui.icon_button(W::new("pl-add-lane"), add, icons::PLUS, false, None);
    ui.tooltip(&b, "Add lane");
    if b.clicked {
        let n = tracks.len() + 1;
        s.doc.edit("Add lane", |p| {
            p.arrangement_mut().tracks.push(Track {
                id: Id::new(),
                name: format!("Track {n}"),
                color: None,
                muted: false,
            })
        });
    }
    ui.scene.pop_clip();
}

/// Corner radius of a clip.
const CLIP_RADIUS: f32 = 3.0;

fn clip_rect(v: &View, lane: usize, c: &Clip) -> Rect {
    let x0 = v.x(c.start.as_f64());
    let x1 = v.x(c.end().as_f64());
    Rect {
        x: x0 + 1.0,
        y: v.lane_y(lane) + 1.0,
        w: (x1 - x0 - 1.0).max(4.0),
        h: v.lane_h - 1.0,
    }
}

fn selected_clips(s: &Session, ws: &Workspace) -> Vec<Clip> {
    s.project()
        .arrangement()
        .clips
        .iter()
        .filter(|c| ws.playlist.selection.contains(&c.id))
        .cloned()
        .collect()
}

fn delete_selection(s: &mut Session, ws: &mut Workspace, label: &str) {
    let selected: HashSet<Id> = selected_clips(s, ws).iter().map(|c| c.id).collect();
    if selected.is_empty() {
        return;
    }
    s.doc.edit(label, |p| {
        p.arrangement_mut()
            .clips
            .retain(|c| !selected.contains(&c.id));
    });
    if let Some(Focus::Clip(id)) = ws.focus
        && selected.contains(&id)
    {
        ws.focus = None;
    }
    ws.playlist.selection.clear();
}

fn copy_selection(s: &Session, ws: &mut Workspace) {
    let lanes: HashMap<Id, usize> = s
        .project()
        .arrangement()
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id, i))
        .collect();
    let copied: Vec<_> = selected_clips(s, ws)
        .into_iter()
        .filter_map(|c| lanes.get(&c.track).copied().map(|lane| (c, lane)))
        .collect();
    if !copied.is_empty() {
        ws.playlist.clipboard = copied;
    }
}

fn paste_clips(s: &mut Session, ws: &mut Workspace, at: Time, lane: usize) {
    let copied = &ws.playlist.clipboard;
    let Some(first) = copied.iter().map(|(c, _)| c.start).min() else {
        return;
    };
    let project = s.project();
    if copied.iter().any(|(c, _)| match c.content {
        ClipContent::Pattern { pattern } => project.pattern(pattern).is_none(),
        ClipContent::Audio { track, .. } => project.mixer.track(track).is_none(),
        ClipContent::Automation { .. } => false,
    }) {
        s.notify("The source of a copied clip was removed", true);
        return;
    }
    let top = copied.iter().map(|(_, lane)| *lane).min().unwrap_or(0);
    let bottom = copied.iter().map(|(_, lane)| *lane).max().unwrap_or(top);
    let height = bottom - top + 1;
    let tracks = &s.project().arrangement().tracks;
    if height > tracks.len() {
        s.notify("Not enough lanes to paste these clips", true);
        return;
    }
    // Near the bottom, shift the entire group up rather than collapsing its lanes.
    let lane = lane.min(tracks.len() - height);
    let at = at.max(Time::ZERO).floor_to(ws.playlist.snap);
    let copies: Vec<Clip> = copied
        .iter()
        .map(|(c, source_lane)| Clip {
            id: Id::new(),
            track: tracks[lane + source_lane - top].id,
            start: at + (c.start - first),
            ..c.clone()
        })
        .collect();
    ws.playlist.selection = copies.iter().map(|c| c.id).collect();
    if let Some(c) = copies
        .iter()
        .find(|c| matches!(c.content, ClipContent::Audio { .. }))
    {
        ws.focus = Some(Focus::Clip(c.id));
    }
    s.doc
        .edit("Paste clips", |p| p.arrangement_mut().clips.extend(copies));
}

fn duplicate_selection(s: &mut Session, ws: &mut Workspace) {
    let selected = selected_clips(s, ws);
    let Some(start) = selected.iter().map(|c| c.start).min() else {
        return;
    };
    let end = selected.iter().map(Clip::end).max().unwrap_or(start);
    let span = end - start;
    // Translate the whole selection equally, preserving gaps, overlaps and lanes.
    let copies: Vec<Clip> = selected
        .iter()
        .map(|c| Clip {
            id: Id::new(),
            start: c.start + span,
            ..c.clone()
        })
        .collect();
    if let Some(Focus::Clip(id)) = ws.focus
        && let Some(i) = selected.iter().position(|c| c.id == id)
    {
        ws.focus = Some(Focus::Clip(copies[i].id));
    }
    ws.playlist.selection = copies.iter().map(|c| c.id).collect();
    s.doc.edit("Duplicate clips", |p| {
        p.arrangement_mut().clips.extend(copies)
    });
}

fn clips(
    ui: &mut Ui,
    v: &View,
    tracks: &[Track],
    clips: &[Clip],
    s: &mut Session,
    ws: &mut Workspace,
) {
    let t = ui.theme.clone();
    let lanes = v.lanes;
    let lane_of: HashMap<Id, usize> = tracks
        .iter()
        .enumerate()
        .map(|(i, tr)| (tr.id, i))
        .collect();
    let patterns: HashMap<Id, Pattern> = s
        .project()
        .patterns
        .iter()
        .map(|p| (p.id, (**p).clone()))
        .collect();
    let channel_order: HashMap<Id, usize> = s
        .project()
        .channels
        .iter()
        .enumerate()
        .map(|(i, c)| (c.id, i))
        .collect();
    let id = W::new("pl-canvas");
    let resp = ui.interact(id, lanes);
    ui.describe(
        id,
        peel_ui::Role::Canvas,
        "Playlist",
        &format!("{} clips on {} lanes", clips.len(), tracks.len()),
    );
    let drag_id = W::new("pl-drag");
    let pointer = ui.pointer();

    let hit = pointer.filter(|p| lanes.contains(*p)).and_then(|p| {
        clips.iter().rev().find_map(|c| {
            let lane = *lane_of.get(&c.track)?;
            let cr = clip_rect(v, lane, c);
            if !cr.contains(p) {
                return None;
            }
            let edge = EDGE.min(cr.w * 0.3);
            let in_header = p.y < cr.y + 16.0;
            let part = match (&c.content, in_header) {
                (
                    ClipContent::Audio {
                        fade_in, fade_out, ..
                    },
                    true,
                ) if (p.x - (v.x((c.start + *fade_in).as_f64()))).abs() <= 6.0 => Part::FadeIn,
                (ClipContent::Audio { fade_out, .. }, true)
                    if (p.x - (v.x((c.end() - *fade_out).as_f64()))).abs() <= 6.0 =>
                {
                    Part::FadeOut
                }
                (ClipContent::Automation { .. }, false) => Part::AutoBody,
                _ if p.x > cr.right() - edge => Part::Right,
                _ if p.x < cr.x + edge => Part::Left,
                _ => Part::Body,
            };
            Some((c.clone(), part))
        })
    });
    if resp.hovered && ui.state::<DragState>(drag_id).kind == Drag::None {
        match (ws.playlist.tool, &hit) {
            (Tool::Erase, _) | (Tool::Slice, _) => ui.set_cursor(CursorIcon::Crosshair),
            (_, Some((_, Part::Body))) => ui.set_cursor(CursorIcon::Grab),
            (_, Some((_, Part::AutoBody))) => ui.set_cursor(CursorIcon::Crosshair),
            (_, Some(_)) => ui.set_cursor(CursorIcon::EwResize),
            _ => {}
        }
    }

    let snap = ws.playlist.snap;
    let pid = current_pattern(s);
    if resp.pressed
        && let Some(p) = pointer
    {
        let beat = v.beat(p.x).max(0.0);
        let lane = v.lane_at(p.y);
        let mut st = DragState {
            start_beat: beat,
            start_lane: lane,
            gesture: id.0 ^ (ui.time() * 1000.0) as u64,
            ..Default::default()
        };
        match (ws.playlist.tool, hit.clone()) {
            (Tool::Erase, _) => st.kind = Drag::Erase,
            (Tool::Slice, Some((c, _))) => {
                let at = Time::from_f64(beat, 960).round_to(snap);
                if at > c.start && at < c.end() {
                    slice_clip(s, &c, at);
                }
            }
            (Tool::Slice, None) => {}
            (tool, Some((c, Part::AutoBody))) if tool != Tool::Erase => {
                let mut return_early = false;
                ws.playlist.selection.clear();
                ws.playlist.selection.insert(c.id);
                if let Some(lane) = lane_of.get(&c.track)
                    && let ClipContent::Automation { points, .. } = &c.content
                {
                    let body = auto_body(clip_rect(v, *lane, &c));
                    let near = points.iter().position(|pt| {
                        let (x, y) = auto_point_xy(v, &c, body, pt);
                        (x - p.x).abs() <= 7.0 && (y - p.y).abs() <= 7.0
                    });
                    let handle = (0..points.len().saturating_sub(1)).find(|&i| {
                        tension_handle(v, &c, body, points, i)
                            .is_some_and(|h| (h.x - p.x).abs() <= 6.0 && (h.y - p.y).abs() <= 6.0)
                    });
                    if near.is_none()
                        && let Some(i) = handle
                    {
                        if resp.double_clicked {
                            let cid = c.id;
                            s.doc.edit("Straighten automation", |p| {
                                if let Some(cl) =
                                    p.arrangement_mut().clips.iter_mut().find(|x| x.id == cid)
                                    && let ClipContent::Automation { points, .. } = &mut cl.content
                                    && let Some(pt) = points.get_mut(i)
                                {
                                    pt.tension = 0.0;
                                }
                            });
                        } else {
                            st.kind = Drag::AutoTension;
                            st.clip = Some(c.clone());
                            st.auto_point = Some(i);
                            st.tension_from = (p.y, points[i].tension);
                        }
                        return_early = true;
                    }
                    let index = match near {
                        Some(i) => i,
                        None if return_early => 0,
                        None => {
                            let grid = if ui.input.modifiers.alt {
                                Time::new(1, 960)
                            } else {
                                auto_snap(snap)
                            };
                            let pos =
                                Time::from_f64(beat - c.start.as_f64() + c.offset.as_f64(), 960)
                                    .round_to(grid)
                                    .max(Time::ZERO);
                            let value = ((body.bottom() - p.y) / body.h).clamp(0.0, 1.0);
                            let at = points.partition_point(|x| x.position <= pos);
                            let cid = c.id;
                            s.doc.edit_gesture(st.gesture, "Add automation point", |p| {
                                if let Some(cl) =
                                    p.arrangement_mut().clips.iter_mut().find(|x| x.id == cid)
                                    && let ClipContent::Automation { points, .. } = &mut cl.content
                                {
                                    points.insert(
                                        at,
                                        peel_document::AutomationPoint::new(pos, value),
                                    );
                                }
                            });
                            at
                        }
                    };
                    if !return_early {
                        st.kind = Drag::AutoPoint;
                        st.clip = Some(c);
                        st.auto_point = Some(index);
                    }
                }
            }
            (_, Some((c, part))) => {
                let edge = part != Part::Body;
                if !ws.playlist.selection.contains(&c.id) {
                    if !ui.input.modifiers.shift {
                        ws.playlist.selection.clear();
                    }
                    ws.playlist.selection.insert(c.id);
                }
                if matches!(c.content, ClipContent::Audio { .. }) {
                    ws.focus = Some(Focus::Clip(c.id));
                }
                st.start_lane = lane;
                let selected: Vec<Clip> = s
                    .project()
                    .arrangement()
                    .clips
                    .iter()
                    .filter(|x| ws.playlist.selection.contains(&x.id))
                    .cloned()
                    .collect();
                // Alt-drag duplicates the selection and moves the copies.
                if ui.input.modifiers.alt && !edge && !resp.double_clicked {
                    let copies: Vec<Clip> = selected
                        .iter()
                        .map(|x| Clip {
                            id: Id::new(),
                            ..x.clone()
                        })
                        .collect();
                    let added = copies.clone();
                    s.doc.edit_gesture(st.gesture, "Duplicate clips", |p| {
                        p.arrangement_mut().clips.extend(added)
                    });
                    ws.playlist.selection = copies.iter().map(|x| x.id).collect();
                    let anchor = copies
                        .iter()
                        .zip(&selected)
                        .find(|(_, o)| o.id == c.id)
                        .map(|(n, _)| n.clone());
                    st.kind = Drag::Move;
                    st.clip = anchor;
                    st.origin = copies;
                    *ui.state::<DragState>(drag_id) = st;
                    return;
                }
                st.origin = selected;
                if resp.double_clicked {
                    if let ClipContent::Pattern { pattern } = c.content {
                        s.select_pattern(pattern);
                        ws.dock = DockTab::Rack;
                    }
                } else {
                    if let ClipContent::Pattern { pattern } = c.content {
                        s.select_pattern(pattern);
                    }
                    st.kind = match part {
                        Part::Body => Drag::Move,
                        Part::Right => Drag::Resize,
                        Part::Left => Drag::TrimStart,
                        Part::FadeIn => Drag::FadeIn,
                        Part::FadeOut => Drag::FadeOut,
                        Part::AutoBody => Drag::Move,
                    };
                    st.clip = Some(c);
                }
            }
            (tool, None) if tool != Tool::Select => {
                if let Some(track) = tracks.get(lane.max(0) as usize)
                    && s.project().pattern(pid).is_some()
                {
                    let start = Time::from_f64(beat, 960).floor_to(snap);
                    let new = place_clip_gesture(s, track.id, start, pid, st.gesture);
                    if tool == Tool::Paint {
                        st.kind = Drag::Paint;
                        st.last_paint = Some((track.id, start));
                    } else {
                        st.kind = Drag::Move;
                        st.clip = new;
                    }
                }
            }
            (Tool::Select, None) => {
                if !ui.input.modifiers.shift {
                    ws.playlist.selection.clear();
                }
                st.kind = Drag::Marquee;
                st.marquee_from = (beat, p.y);
            }
            _ => ws.playlist.selection.clear(),
        }
        *ui.state::<DragState>(drag_id) = st;
    }

    // Right click opens the clip menu.
    let menu_id = W::new("pl-clip-menu");
    let context_target = W::new("pl-context-target");
    if resp.secondary_clicked
        && let Some(p) = pointer
    {
        *ui.state::<Option<(Time, usize)>>(context_target) = Some((
            Time::from_f64(v.beat(p.x).max(0.0), 960).floor_to(snap),
            v.lane_at(p.y).max(0) as usize,
        ));
    }
    let mut deleted_point = false;
    if resp.secondary_clicked
        && let Some((c, Part::AutoBody)) = &hit
        && let Some(lane) = lane_of.get(&c.track)
        && let ClipContent::Automation { points, .. } = &c.content
        && let Some(p) = pointer
    {
        let body = auto_body(clip_rect(v, *lane, c));
        if let Some(i) = points.iter().position(|pt| {
            let (x, y) = auto_point_xy(v, c, body, pt);
            (x - p.x).abs() <= 7.0 && (y - p.y).abs() <= 7.0
        }) {
            let cid = c.id;
            s.doc.edit("Delete automation point", |p| {
                if let Some(cl) = p.arrangement_mut().clips.iter_mut().find(|x| x.id == cid)
                    && let ClipContent::Automation { points, .. } = &mut cl.content
                    && points.len() > 1
                {
                    points.remove(i);
                }
            });
            deleted_point = true;
        }
    }
    if resp.secondary_clicked
        && !deleted_point
        && let Some((c, _)) = &hit
    {
        if !ws.playlist.selection.contains(&c.id) {
            ws.playlist.selection.clear();
            ws.playlist.selection.insert(c.id);
        }
        *ui.state::<Option<Clip>>(menu_id) = Some(c.clone());
        ui.open_popup(
            menu_id,
            Rect::from_center(pointer.unwrap_or_default(), 1.0, 1.0),
        );
    }
    if let Some(c) = ui.state::<Option<Clip>>(menu_id).clone() {
        let at = ui.popup_rect().map(|r| vec2(r.x, r.y)).unwrap_or_default();
        let selected = selected_clips(s, ws);
        let multiple = selected.len() > 1;
        let muted = !selected.is_empty() && selected.iter().all(|c| c.muted);
        let cmd = if cfg!(target_os = "macos") {
            "Cmd"
        } else {
            "Ctrl"
        };
        let mut entries = Vec::new();
        if !multiple {
            match c.content {
                ClipContent::Pattern { pattern } => {
                    entries.extend([
                        (MenuItem::new("Open pattern"), ClipAction::Open),
                        (
                            MenuItem::new("Make unique")
                                .enabled(s.project().pattern_usage(pattern) > 1),
                            ClipAction::Unique,
                        ),
                        (MenuItem::new("Rename pattern"), ClipAction::Rename),
                    ]);
                }
                ClipContent::Audio { reversed, .. } => entries.push((
                    MenuItem::new(if reversed { "Play forwards" } else { "Reverse" }),
                    ClipAction::Reverse,
                )),
                ClipContent::Automation { .. } => {}
            }
        }
        entries.extend([
            (
                MenuItem::new(match (muted, multiple) {
                    (true, true) => "Unmute clips",
                    (true, false) => "Unmute clip",
                    (false, true) => "Mute clips",
                    (false, false) => "Mute clip",
                }),
                ClipAction::Mute,
            ),
            (MenuItem::Separator, ClipAction::None),
            (
                MenuItem::new("Copy").shortcut(format!("{cmd}+C")),
                ClipAction::Copy,
            ),
            (
                MenuItem::new("Cut").shortcut(format!("{cmd}+X")),
                ClipAction::Cut,
            ),
            (
                MenuItem::new(if multiple {
                    "Duplicate clips"
                } else {
                    "Duplicate clip"
                })
                .shortcut(format!("{cmd}+D")),
                ClipAction::Duplicate,
            ),
            (
                MenuItem::new(if multiple {
                    "Delete clips"
                } else {
                    "Delete clip"
                })
                .shortcut("Del"),
                ClipAction::Delete,
            ),
        ]);
        if !multiple {
            match c.content {
                ClipContent::Pattern { .. } => entries.extend([
                    (MenuItem::Separator, ClipAction::None),
                    (
                        MenuItem::new("Render to audio").enabled(s.render_progress().is_none()),
                        ClipAction::Render,
                    ),
                ]),
                ClipContent::Audio { reversed, .. } => entries.extend([
                    (MenuItem::Separator, ClipAction::None),
                    (
                        MenuItem::new("Chop at hits").enabled(!reversed),
                        ClipAction::Chop,
                    ),
                ]),
                ClipContent::Automation { .. } => {}
            }
        }
        let items: Vec<MenuItem> = entries.iter().map(|(item, _)| item.clone()).collect();
        if let Some(choice) = ui.menu(menu_id, at, &items) {
            match entries[choice].1 {
                ClipAction::Open => {
                    if let ClipContent::Pattern { pattern } = c.content {
                        s.select_pattern(pattern);
                        ws.dock = DockTab::Rack;
                    }
                }
                ClipAction::Unique => {
                    if let ClipContent::Pattern { pattern } = c.content
                        && let Some(new) = make_unique(s, c.id, pattern)
                    {
                        s.select_pattern(new);
                    }
                }
                ClipAction::Rename => {
                    if let ClipContent::Pattern { pattern } = c.content {
                        let name = s
                            .project()
                            .pattern(pattern)
                            .map(|p| p.name.clone())
                            .unwrap_or_default();
                        let lane = lane_of.get(&c.track).copied().unwrap_or(0);
                        let anchor = clip_rect(v, lane, &c);
                        *ui.state::<Option<Id>>(W::new("pl-rename-target")) = Some(pattern);
                        crate::rename::open(ui, W::new("pl-rename-pattern"), anchor, &name);
                    }
                }
                ClipAction::Mute => {
                    let sel = ws.playlist.selection.clone();
                    s.doc
                        .edit(if muted { "Unmute clips" } else { "Mute clips" }, |p| {
                            for clip in p
                                .arrangement_mut()
                                .clips
                                .iter_mut()
                                .filter(|c| sel.contains(&c.id))
                            {
                                clip.muted = !muted;
                            }
                        });
                }
                ClipAction::Copy => copy_selection(s, ws),
                ClipAction::Cut => {
                    copy_selection(s, ws);
                    delete_selection(s, ws, "Cut clips");
                }
                ClipAction::Duplicate => duplicate_selection(s, ws),
                ClipAction::Delete => delete_selection(s, ws, "Delete clips"),
                ClipAction::Reverse => {
                    let cid = c.id;
                    s.doc.edit("Reverse clip", |p| {
                        if let Some(c) = p.arrangement_mut().clips.iter_mut().find(|c| c.id == cid)
                            && let ClipContent::Audio { reversed, .. } = &mut c.content
                        {
                            *reversed = !*reversed;
                        }
                    });
                }
                ClipAction::Render => {
                    if let Err(e) = s.render_clip_to_audio(c.id) {
                        s.notify(e, true);
                    }
                }
                ClipAction::Chop => match chop_at_hits(s, &c) {
                    Some(n) => s.notify(format!("Chopped into {n} clips"), false),
                    None => s.notify("No hits found in this clip", true),
                },
                ClipAction::None => {}
            }
        }
        if !ui.is_popup_open(menu_id) {
            *ui.state::<Option<Clip>>(menu_id) = None;
        }
    }
    // Empty-grid menu: keep the clicked position while the menu moves the pointer.
    let canvas_menu = W::new("pl-canvas-menu");
    if resp.secondary_clicked
        && hit.is_none()
        && let Some(p) = pointer
    {
        let at = Time::from_f64(v.beat(p.x).max(0.0), 960).floor_to(snap);
        let lane = (v.lane_at(p.y).max(0) as usize).min(tracks.len().saturating_sub(1));
        *ui.state::<Option<(Time, usize)>>(canvas_menu) = Some((at, lane));
        ui.open_popup(canvas_menu, Rect::from_center(p, 1.0, 1.0));
    }
    if let Some((at, lane)) = *ui.state::<Option<(Time, usize)>>(canvas_menu) {
        let pos = ui.popup_rect().map(|r| vec2(r.x, r.y)).unwrap_or_default();
        let cmd = if cfg!(target_os = "macos") {
            "Cmd"
        } else {
            "Ctrl"
        };
        let items = [
            MenuItem::new("Paste")
                .shortcut(format!("{cmd}+V"))
                .enabled(!ws.playlist.clipboard.is_empty()),
            MenuItem::Separator,
            MenuItem::new("Select all")
                .shortcut(format!("{cmd}+A"))
                .enabled(!s.project().arrangement().clips.is_empty()),
        ];
        match ui.menu(canvas_menu, pos, &items) {
            Some(0) => paste_clips(s, ws, at, lane),
            Some(2) => {
                ws.playlist.selection = s
                    .project()
                    .arrangement()
                    .clips
                    .iter()
                    .map(|c| c.id)
                    .collect();
            }
            _ => {}
        }
        if !ui.is_popup_open(canvas_menu) {
            *ui.state::<Option<(Time, usize)>>(canvas_menu) = None;
        }
    }
    if let Some(name) = crate::rename::show(ui, W::new("pl-rename-pattern"))
        && let Some(pid) = *ui.state::<Option<Id>>(W::new("pl-rename-target"))
    {
        s.doc.edit("Rename pattern", |p| {
            if let Some(pt) = p.pattern_mut(pid) {
                pt.name = name;
            }
        });
    }

    let st = ui.state::<DragState>(drag_id).clone();
    if st.kind != Drag::None && ui.input.down[0] {
        if let Some(p) = pointer {
            let beat = v.beat(p.x);
            match st.kind {
                Drag::Move => {
                    if let Some(orig) = &st.clip {
                        let raw = orig.start.as_f64() + (beat - st.start_beat);
                        let start = Time::from_f64(raw.max(0.0), 960)
                            .round_to(snap)
                            .max(Time::ZERO);
                        let mut dt = start - orig.start;
                        let mut dl = v.lane_at(p.y) - st.start_lane;
                        let origin = if st.origin.is_empty() {
                            vec![orig.clone()]
                        } else {
                            st.origin.clone()
                        };
                        // Keep the whole group inside the arrangement.
                        let min_start = origin.iter().map(|c| c.start).min().unwrap_or(Time::ZERO);
                        if min_start + dt < Time::ZERO {
                            dt = -min_start;
                        }
                        let lanes: Vec<i32> = origin
                            .iter()
                            .filter_map(|c| tracks.iter().position(|t| t.id == c.track))
                            .map(|i| i as i32)
                            .collect();
                        let (lo, hi) = (
                            lanes.iter().copied().min().unwrap_or(0),
                            lanes.iter().copied().max().unwrap_or(0),
                        );
                        dl = dl.clamp(-lo, tracks.len() as i32 - 1 - hi);
                        let moves: Vec<(Id, Time, Id)> = origin
                            .iter()
                            .filter_map(|c| {
                                let li = tracks.iter().position(|t| t.id == c.track)? as i32 + dl;
                                Some((c.id, c.start + dt, tracks[li as usize].id))
                            })
                            .collect();
                        let label = if moves.len() == 1 {
                            "Move clip".to_string()
                        } else {
                            format!("Move {} clips", moves.len())
                        };
                        s.doc.edit_gesture(st.gesture, label, |p| {
                            for (id, start, track) in &moves {
                                if let Some(c) =
                                    p.arrangement_mut().clips.iter_mut().find(|c| c.id == *id)
                                {
                                    c.start = *start;
                                    c.track = *track;
                                }
                            }
                        });
                        ui.set_cursor(CursorIcon::Grabbing);
                    }
                }
                Drag::AutoTension => {
                    if let (Some(orig), Some(i)) = (&st.clip, st.auto_point)
                        && let ClipContent::Automation { points, .. } = &orig.content
                        && let (Some(a), Some(b)) = (points.get(i), points.get(i + 1))
                    {
                        // Moving the pointer down lowers the middle of the
                        // segment, whichever way it slopes.
                        let (y0, k0) = st.tension_from;
                        let dir = if b.value > a.value { 1.0 } else { -1.0 };
                        let k = (k0 + (p.y - y0) / 80.0 * dir).clamp(-1.0, 1.0);
                        let cid = orig.id;
                        s.doc.edit_gesture(st.gesture, "Bend automation", |p| {
                            if let Some(cl) =
                                p.arrangement_mut().clips.iter_mut().find(|x| x.id == cid)
                                && let ClipContent::Automation { points, .. } = &mut cl.content
                                && let Some(pt) = points.get_mut(i)
                            {
                                pt.tension = k;
                            }
                        });
                    }
                }
                Drag::AutoPoint => {
                    if let (Some(orig), Some(i)) = (&st.clip, st.auto_point)
                        && let Some(lane) = lane_of.get(&orig.track)
                        && let ClipContent::Automation { points, .. } = &orig.content
                    {
                        let body = auto_body(clip_rect(v, *lane, orig));
                        let grid = if ui.input.modifiers.alt {
                            Time::new(1, 960)
                        } else {
                            auto_snap(snap)
                        };
                        let lo = if i > 0 {
                            points[i - 1].position
                        } else {
                            Time::ZERO
                        };
                        let hi = points
                            .get(i + 1)
                            .map(|x| x.position)
                            .unwrap_or(orig.offset + orig.length);
                        let pos =
                            Time::from_f64(beat - orig.start.as_f64() + orig.offset.as_f64(), 960)
                                .round_to(grid)
                                .clamp(lo, hi);
                        let value = ((body.bottom() - p.y) / body.h).clamp(0.0, 1.0);
                        let cid = orig.id;
                        s.doc
                            .edit_gesture(st.gesture, "Move automation point", |p| {
                                if let Some(cl) =
                                    p.arrangement_mut().clips.iter_mut().find(|x| x.id == cid)
                                    && let ClipContent::Automation { points, .. } = &mut cl.content
                                    && let Some(pt) = points.get_mut(i)
                                {
                                    pt.position = pos;
                                    pt.value = value;
                                }
                            });
                    }
                }
                Drag::TrimStart => {
                    if let Some(orig) = &st.clip {
                        let raw = orig.start.as_f64() + (beat - st.start_beat);
                        let lo = orig.start - orig.offset;
                        let hi = orig.end() - snap;
                        let new_start = Time::from_f64(raw.max(0.0), 960)
                            .round_to(snap)
                            .clamp(lo.max(Time::ZERO), hi);
                        let d = new_start - orig.start;
                        let cid = orig.id;
                        let (o_off, o_len) = (orig.offset, orig.length);
                        s.doc.edit_gesture(st.gesture, "Trim clip", |p| {
                            if let Some(c) =
                                p.arrangement_mut().clips.iter_mut().find(|c| c.id == cid)
                            {
                                c.start = new_start;
                                c.offset = o_off + d;
                                c.length = o_len - d;
                            }
                        });
                        ui.set_cursor(CursorIcon::EwResize);
                    }
                }
                Drag::FadeIn | Drag::FadeOut => {
                    if let Some(orig) = &st.clip {
                        let fine = if ui.input.modifiers.alt {
                            Time::new(1, 64)
                        } else {
                            Time::new(1, 16)
                        };
                        let cid = orig.id;
                        let fade_in_kind = st.kind == Drag::FadeIn;
                        let raw = if fade_in_kind {
                            beat - orig.start.as_f64()
                        } else {
                            orig.end().as_f64() - beat
                        };
                        let value = Time::from_f64(raw.max(0.0), 960)
                            .round_to(fine)
                            .max(Time::ZERO);
                        s.doc.edit_gesture(
                            st.gesture,
                            if fade_in_kind {
                                "Change fade in"
                            } else {
                                "Change fade out"
                            },
                            |p| {
                                if let Some(c) =
                                    p.arrangement_mut().clips.iter_mut().find(|c| c.id == cid)
                                {
                                    let len = c.length;
                                    if let ClipContent::Audio {
                                        fade_in, fade_out, ..
                                    } = &mut c.content
                                    {
                                        if fade_in_kind {
                                            *fade_in = value.min(len - *fade_out).max(Time::ZERO);
                                        } else {
                                            *fade_out = value.min(len - *fade_in).max(Time::ZERO);
                                        }
                                    }
                                }
                            },
                        );
                        ui.set_cursor(CursorIcon::EwResize);
                    }
                }
                Drag::Marquee => {
                    let (b0, y0) = st.marquee_from;
                    let x0 = v.x(b0);
                    let mr = Rect::from_min_max(
                        vec2(x0.min(p.x), y0.min(p.y)),
                        vec2(x0.max(p.x), y0.max(p.y)),
                    );
                    ui.with_layer(peel_ui::LayerId::ABOVE, |ui| {
                        ui.scene.push_clip(lanes);
                        ui.scene
                            .rect_stroke(mr, 2.0, t.focus.alpha(0.12), 1.0, t.focus.alpha(0.8));
                        ui.scene.pop_clip();
                    });
                    let inside: Vec<Id> = s
                        .project()
                        .arrangement()
                        .clips
                        .iter()
                        .filter(|c| {
                            lane_of
                                .get(&c.track)
                                .is_some_and(|&l| clip_rect(v, l, c).intersects(&mr))
                        })
                        .map(|c| c.id)
                        .collect();
                    if !ui.input.modifiers.shift {
                        ws.playlist.selection.clear();
                    }
                    ws.playlist.selection.extend(inside);
                }
                Drag::Resize => {
                    if let Some(orig) = &st.clip {
                        let raw_end = orig.end().as_f64() + (beat - st.start_beat);
                        let end = Time::from_f64(raw_end, 960).round_to(snap);
                        let len = (end - orig.start).max(snap);
                        let cid = orig.id;
                        s.doc.edit_gesture(st.gesture, "Resize clip", |p| {
                            if let Some(c) =
                                p.arrangement_mut().clips.iter_mut().find(|c| c.id == cid)
                            {
                                c.length = len;
                            }
                        });
                        ui.set_cursor(CursorIcon::EwResize);
                    }
                }
                Drag::Paint => {
                    let lane = v.lane_at(p.y);
                    if let (Some(track), Some(pat)) = (
                        tracks.get(lane.max(0) as usize),
                        s.project().pattern(pid).cloned(),
                    ) {
                        let len = pat.length;
                        let start = Time::from_f64(beat.max(0.0), 960).floor_to(len.max(snap));
                        let occupied = s.project().arrangement().clips.iter().any(|c| {
                            c.track == track.id && c.start < start + len && c.end() > start
                        });
                        if !occupied && st.last_paint != Some((track.id, start)) {
                            place_clip_gesture(s, track.id, start, pid, st.gesture);
                            ui.state::<DragState>(drag_id).last_paint = Some((track.id, start));
                        }
                    }
                }
                Drag::Erase => {
                    if let Some((c, _)) = &hit {
                        let cid = c.id;
                        s.doc.edit_gesture(st.gesture, "Delete clips", |p| {
                            p.arrangement_mut().clips.retain(|x| x.id != cid)
                        });
                    }
                }
                _ => {}
            }
        }
    } else if st.kind != Drag::None {
        ui.state::<DragState>(drag_id).kind = Drag::None;
        s.doc.end_gesture();
    }

    // The pointer or an open arrangement menu chooses the editor. Consume keys
    // before the piano roll sees them, including commands on an empty selection.
    let own_menu = ui.is_popup_open(menu_id) || ui.is_popup_open(canvas_menu);
    if !ui.text_focus() && (own_menu || (!ui.any_popup() && ui.pointer_in(lanes))) {
        let all = ui.input.take_command("a", false);
        let copy = ui.input.take_command("c", false);
        let cut = ui.input.take_command("x", false);
        let paste = ui.input.take_command("v", false);
        let duplicate = ui.input.take_command("d", false);
        let delete = ui.input.take_named(peel_ui::NamedKey::Delete)
            || ui.input.take_named(peel_ui::NamedKey::Backspace);
        if all {
            ws.playlist.selection = s
                .project()
                .arrangement()
                .clips
                .iter()
                .map(|c| c.id)
                .collect();
        }
        if copy || cut {
            copy_selection(s, ws);
        }
        if cut {
            delete_selection(s, ws, "Cut clips");
        }
        if paste {
            let target = if own_menu {
                *ui.state::<Option<(Time, usize)>>(context_target)
            } else {
                pointer.map(|p| {
                    (
                        Time::from_f64(v.beat(p.x).max(0.0), 960),
                        v.lane_at(p.y).max(0) as usize,
                    )
                })
            };
            if let Some((at, lane)) = target {
                paste_clips(s, ws, at, lane);
            }
        }
        if delete {
            delete_selection(s, ws, "Delete clips");
        }
        if duplicate {
            duplicate_selection(s, ws);
        }
        if own_menu && (all || copy || cut || paste || duplicate || delete) {
            ui.close_popup();
        }
    }

    // Draw clips.
    let fresh = s.project().arrangement().clips.clone();
    ui.scene.push_clip(lanes);
    for c in &fresh {
        let Some(&lane) = lane_of.get(&c.track) else {
            continue;
        };
        let cr = clip_rect(v, lane, c);
        if !cr.intersects(&lanes) {
            continue;
        }
        let lane_muted_any = tracks.get(lane).is_some_and(|t| t.muted) || c.muted;
        let pattern = match c.content {
            ClipContent::Pattern { pattern } => pattern,
            ClipContent::Automation {
                ref target,
                ref points,
            } => {
                let sel = ws.playlist.selection.contains(&c.id);
                let hovered = hit.as_ref().is_some_and(|(h, _)| h.id == c.id);
                draw_automation_clip(
                    ui,
                    v,
                    c,
                    cr,
                    target,
                    points,
                    sel,
                    hovered,
                    lane_muted_any,
                    s,
                );
                continue;
            }
            ClipContent::Audio {
                asset, reversed, ..
            } => {
                let sel = ws.playlist.selection.contains(&c.id);
                let hovered = hit.as_ref().is_some_and(|(h, _)| h.id == c.id);
                draw_audio_clip(
                    ui,
                    v,
                    c,
                    cr,
                    asset,
                    reversed,
                    sel,
                    hovered,
                    lane_muted_any,
                    s,
                );
                continue;
            }
        };
        let Some(pat) = patterns.get(&pattern) else {
            continue;
        };
        let pc = col(pat.color);
        let sel = ws.playlist.selection.contains(&c.id);
        let hovered = hit.as_ref().is_some_and(|(h, _)| h.id == c.id);
        let lane_muted = tracks.get(lane).is_some_and(|t| t.muted) || c.muted;
        // A dark body tinted with the pattern color under a full-color
        // name strip, so the notes inside are the brightest part.
        let body = t.bg.mix(pc, if lane_muted { 0.12 } else { 0.32 });
        ui.scene.rect(
            cr,
            CLIP_RADIUS,
            if hovered { body.lighten(0.05) } else { body },
        );
        let header = Rect {
            h: 16.0f32.min(cr.h * 0.4),
            ..cr
        };
        ui.scene.rect_ex(
            header,
            [CLIP_RADIUS, CLIP_RADIUS, 0.0, 0.0],
            if lane_muted { t.bg.mix(pc, 0.4) } else { pc },
            0.0,
            Color::TRANSPARENT,
        );
        if cr.w > 26.0 {
            ui.with_clip(header, |ui| {
                ui.text(
                    header.shrink2(6.0, 0.0),
                    &pat.name,
                    t.caption.weight(peel_ui::FontWeight::SemiBold),
                    ink_on(pc),
                    Align::Start,
                );
            });
        }
        // Notes preview from real pattern data, repeated per loop.
        let notes_r = Rect {
            y: header.bottom() + 3.0,
            h: cr.bottom() - header.bottom() - 6.0,
            ..cr.shrink2(3.0, 0.0)
        };
        if notes_r.h > 6.0 && !pat.notes.is_empty() {
            // One preview row per (channel, key): melodic parts read as
            // pitch, drum parts as separate lanes.
            let mut rows: Vec<(usize, u8)> = pat
                .notes
                .iter()
                .map(|n| {
                    (
                        channel_order.get(&n.channel).copied().unwrap_or(usize::MAX),
                        n.key,
                    )
                })
                .collect();
            rows.sort_unstable();
            rows.dedup();
            let melodic = rows.len() > 12 || rows.windows(2).all(|w| w[0].0 == w[1].0);
            let (lo, hi) = pat
                .notes
                .iter()
                .fold((127u8, 0u8), |(lo, hi), n| (lo.min(n.key), hi.max(n.key)));
            let span = if melodic {
                (hi as f32 - lo as f32 + 1.0).max(6.0)
            } else {
                rows.len() as f32
            };
            let row_of = |n: &peel_document::Note| -> f32 {
                if melodic {
                    (n.key - lo) as f32
                } else {
                    let key = (
                        channel_order.get(&n.channel).copied().unwrap_or(usize::MAX),
                        n.key,
                    );
                    (rows.len() - 1 - rows.binary_search(&key).unwrap_or(0)) as f32
                }
            };
            let l = pat.length.as_f64();
            ui.scene.push_clip(cr);
            let mut k = 0i64;
            loop {
                let origin = c.start.as_f64() - c.offset.as_f64() + k as f64 * l;
                if origin >= c.end().as_f64() || k > 512 {
                    break;
                }
                if k > 0 {
                    let x = v.x(origin);
                    ui.scene
                        .vline(x, notes_r.y, notes_r.bottom(), Color::WHITE.alpha(0.08));
                }
                for n in &pat.notes {
                    let b0 = origin + n.start.as_f64();
                    if b0 < c.start.as_f64() || b0 >= c.end().as_f64() {
                        continue;
                    }
                    let x0 = v.x(b0);
                    let x1 = v.x((b0 + n.length.as_f64()).min(c.end().as_f64()));
                    let y = notes_r.bottom() - notes_r.h * (row_of(n) + 0.5) / span;
                    let nh = (notes_r.h / span * 0.7).clamp(1.5, 5.0);
                    let alpha = if lane_muted {
                        0.3
                    } else {
                        0.55 + 0.4 * n.velocity
                    };
                    ui.scene.rect(
                        Rect {
                            x: x0,
                            y: y - nh * 0.5,
                            w: (x1 - x0 - 1.0).max(1.5),
                            h: nh,
                        },
                        1.0,
                        pc.lighten(0.4).alpha(alpha),
                    );
                }
                k += 1;
            }
            ui.scene.pop_clip();
        }
        if sel {
            ui.scene.outline(cr, CLIP_RADIUS, 1.5, t.text);
        }
        if hovered && !sel {
            ui.scene
                .outline(cr, CLIP_RADIUS, 1.0, pc.lighten(0.3).alpha(0.6));
        }
    }
    ui.scene.pop_clip();
}

fn place_clip(s: &mut Session, track: Id, start: Time, pattern: Id) {
    let gesture = W::new(("place", track, start.as_f64().to_bits())).0;
    place_clip_gesture(s, track, start, pattern, gesture);
    s.doc.end_gesture();
}

fn place_clip_gesture(
    s: &mut Session,
    track: Id,
    start: Time,
    pattern: Id,
    gesture: u64,
) -> Option<Clip> {
    let len = s.project().pattern(pattern)?.length;
    let clip = Clip {
        id: Id::new(),
        track,
        start,
        length: len,
        offset: Time::ZERO,
        muted: false,
        content: ClipContent::Pattern { pattern },
    };
    let out = clip.clone();
    s.doc.edit_gesture(gesture, "Place pattern", |p| {
        p.arrangement_mut().clips.push(clip)
    });
    Some(out)
}

/// Gives one clip its own copy of a shared pattern. Returns the new
/// pattern id.
pub fn make_unique(s: &mut Session, clip: Id, pattern: Id) -> Option<Id> {
    let src = s.project().pattern(pattern)?.as_ref().clone();
    let existing: Vec<String> = s
        .project()
        .patterns
        .iter()
        .map(|p| p.name.clone())
        .collect();
    let mut n = 2;
    let base = src
        .name
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end()
        .to_string();
    let mut name = format!("{base} {n}");
    while existing.contains(&name) {
        n += 1;
        name = format!("{base} {n}");
    }
    let mut copy = src;
    copy.id = Id::new();
    copy.name = name;
    let mut map = HashMap::new();
    for note in &mut copy.notes {
        let id = Id::new();
        map.insert(note.id, id);
        note.id = id;
    }
    copy.expression = crate::note_expr::remap(&copy.expression, &map);
    let new_id = copy.id;
    s.doc.edit("Make unique", |p| {
        p.patterns.push(std::sync::Arc::new(copy));
        if let Some(c) = p.arrangement_mut().clips.iter_mut().find(|c| c.id == clip) {
            c.content = ClipContent::Pattern { pattern: new_id };
        }
    });
    Some(new_id)
}

/// Draws an audio clip with its waveform from the cached peak pyramid.
#[allow(clippy::too_many_arguments)]
fn draw_audio_clip(
    ui: &mut Ui,
    v: &View,
    c: &Clip,
    cr: Rect,
    asset: Id,
    reversed: bool,
    sel: bool,
    hovered: bool,
    muted: bool,
    s: &Session,
) {
    let t = ui.theme.clone();
    let wave = col(peel_document::PALETTE[5]);
    let body = t.bg.mix(wave, if muted { 0.10 } else { 0.24 });
    ui.scene.rect(
        cr,
        CLIP_RADIUS,
        if hovered { body.lighten(0.05) } else { body },
    );
    let header = Rect {
        h: 16.0f32.min(cr.h * 0.4),
        ..cr
    };
    ui.scene.rect_ex(
        header,
        [CLIP_RADIUS, CLIP_RADIUS, 0.0, 0.0],
        if muted { t.bg.mix(wave, 0.4) } else { wave },
        0.0,
        Color::TRANSPARENT,
    );
    let name = s
        .project()
        .asset(asset)
        .map(|a| a.name.clone())
        .unwrap_or_default();
    if cr.w > 26.0 {
        let label = if reversed {
            format!("{name}  (reversed)")
        } else {
            name
        };
        ui.with_clip(header, |ui| {
            ui.text(
                header.shrink2(6.0, 0.0),
                &label,
                t.caption.weight(peel_ui::FontWeight::SemiBold),
                ink_on(wave),
                Align::Start,
            );
        });
    }
    let body_r = Rect {
        y: header.bottom() + 2.0,
        h: cr.bottom() - header.bottom() - 4.0,
        ..cr
    };
    if body_r.h < 4.0 {
        return;
    }
    let Some(peaks) = s.audio_peaks(asset) else {
        if s.audio_loading(asset) {
            ui.text(body_r, "Loading", t.caption, t.text_faint, Align::Center);
        } else {
            ui.text(body_r, "Missing file", t.caption, t.record, Align::Center);
        }
        return;
    };
    // Map each pixel column to source frames through the tempo map.
    let tempo = peel_time::TempoMap::from_segments(&s.project().tempo);
    let rate = s.sample_rate() as f64;
    let source0 = peel_session::clip_source_start_seconds(&tempo, c);
    let clip_sec0 = tempo.beat_to_seconds(c.start.as_f64());
    let clip_sec1 = tempo.beat_to_seconds(c.end().as_f64());
    let frames = peaks.frames() as f64;
    let x0 = cr.x.max(v.lanes.x).floor();
    let x1 = cr.right().min(v.lanes.right()).ceil();
    let cols = (x1 - x0).max(1.0) as usize;
    let mid = body_r.center().y;
    let amp = body_r.h * 0.48;
    ui.scene.push_clip(cr);
    // A clip following the song tempo reads the file in its own beats.
    let follow = match &c.content {
        ClipContent::Audio { source_bpm, .. } => *source_bpm,
        _ => None,
    };
    let frame_at = |x: f32| -> f64 {
        let into = if let Some(src) = follow {
            let b = v.beat(x);
            let beats = if reversed {
                c.end().as_f64() - b
            } else {
                b - c.start.as_f64()
            };
            (beats + c.offset.as_f64()) * 60.0 / src
        } else {
            let sec = tempo.beat_to_seconds(v.beat(x));
            if reversed {
                (clip_sec1 - sec) + source0
            } else {
                (sec - clip_sec0) + source0
            }
        };
        (into * rate).clamp(0.0, frames)
    };
    let (mut prev_end, color) = (
        frame_at(x0),
        wave.lighten(0.3).alpha(if muted { 0.35 } else { 0.9 }),
    );
    for i in 0..cols {
        let x = x0 + i as f32;
        let f_end = frame_at(x + 1.0);
        let (a, b) = if prev_end <= f_end {
            (prev_end, f_end)
        } else {
            (f_end, prev_end)
        };
        prev_end = f_end;
        if b - a < 1.0 || a >= frames {
            continue;
        }
        if let Some((lo, hi)) = peaks
            .query(a as usize, (b as usize).max(a as usize + 1), 1)
            .next()
        {
            let y0 = mid - hi.clamp(-1.0, 1.0) * amp;
            let y1 = mid - lo.clamp(-1.0, 1.0) * amp;
            ui.scene.rect(
                Rect {
                    x,
                    y: y0,
                    w: 1.0,
                    h: (y1 - y0).max(1.0),
                },
                0.0,
                color,
            );
        }
    }
    // Fades: shade the attenuated part, draw the ramp and its handle.
    if let ClipContent::Audio {
        fade_in, fade_out, ..
    } = c.content
    {
        let fi = v.x((c.start + fade_in).as_f64()) - cr.x;
        let fo = cr.right() - v.x((c.end() - fade_out).as_f64());
        let shade = t.bg.alpha(0.55);
        if fi > 1.0 {
            for i in 0..fi as usize {
                let g = i as f32 / fi;
                ui.scene.rect(
                    Rect {
                        x: cr.x + i as f32,
                        y: body_r.y,
                        w: 1.0,
                        h: body_r.h,
                    },
                    0.0,
                    shade.alpha(0.55 * (1.0 - g)),
                );
            }
            ui.scene.line(
                vec2(cr.x, body_r.bottom()),
                vec2(cr.x + fi, body_r.y),
                1.5,
                Color::WHITE.alpha(0.7),
            );
        }
        if fo > 1.0 {
            for i in 0..fo as usize {
                let g = i as f32 / fo;
                ui.scene.rect(
                    Rect {
                        x: cr.right() - i as f32 - 1.0,
                        y: body_r.y,
                        w: 1.0,
                        h: body_r.h,
                    },
                    0.0,
                    shade.alpha(0.55 * (1.0 - g)),
                );
            }
            ui.scene.line(
                vec2(cr.right() - fo, body_r.y),
                vec2(cr.right(), body_r.bottom()),
                1.5,
                Color::WHITE.alpha(0.7),
            );
        }
        if hovered || sel || fi > 1.0 || fo > 1.0 {
            for x in [cr.x + fi, cr.right() - fo] {
                ui.scene.rect(
                    Rect::from_center(
                        vec2(x.clamp(cr.x + 4.0, cr.right() - 4.0), header.bottom() - 1.0),
                        7.0,
                        7.0,
                    ),
                    1.5,
                    Color::WHITE.alpha(0.9),
                );
            }
        }
    }
    ui.scene.pop_clip();
    if sel {
        ui.scene
            .outline(cr, t.radius_s, 1.5, Color::WHITE.alpha(0.85));
    }
}

/// The envelope area of an automation clip (below its header).
fn auto_body(cr: Rect) -> Rect {
    let header = 16.0f32.min(cr.h * 0.4);
    Rect {
        y: cr.y + header + 4.0,
        h: (cr.h - header - 8.0).max(4.0),
        ..cr
    }
}

fn auto_point_xy(
    v: &View,
    c: &Clip,
    body: Rect,
    pt: &peel_document::AutomationPoint,
) -> (f32, f32) {
    let beat = (c.start + pt.position - c.offset).as_f64();
    (v.x(beat), body.bottom() - pt.value * body.h)
}

/// Breakpoints snap four times finer than the playlist grid.
fn auto_snap(snap: Time) -> Time {
    snap.div_int(4).max(Time::new(1, 16))
}

/// A readable name for what an automation clip controls.
pub fn automation_label(s: &Session, target: &peel_document::AutomationTarget) -> (String, Color) {
    use peel_document::AutomationTarget as T;
    let p = s.project();
    match target {
        T::Param { device, param } => {
            let pname = peel_session::find_device(p, *device)
                .and_then(|d| peel_devices::descriptor(&d.type_id))
                .and_then(|d| d.param_by_key(param))
                .map_or(param.as_str(), |pd| pd.name);
            if let Some(ch) = p.channels.iter().find(|c| c.device.id == *device) {
                return (format!("{} · {pname}", ch.name), col(ch.color));
            }
            if let Some(t) = p
                .mixer
                .tracks
                .iter()
                .find(|t| t.inserts.iter().any(|d| d.id == *device))
            {
                let dev = t
                    .inserts
                    .iter()
                    .find(|d| d.id == *device)
                    .and_then(|d| peel_devices::descriptor(&d.type_id))
                    .map_or("", |d| d.name);
                return (format!("{} {dev} · {pname}", t.name), col(t.color));
            }
            (format!("Missing · {pname}"), Color::hex(0x95A2B0))
        }
        T::TrackGain { track } | T::TrackPan { track } => {
            let what = if matches!(target, T::TrackGain { .. }) {
                "Volume"
            } else {
                "Pan"
            };
            match p.mixer.track(*track) {
                Some(t) => (format!("{} · {what}", t.name), col(t.color)),
                None => (format!("Missing · {what}"), Color::hex(0x95A2B0)),
            }
        }
        T::MidiCc { device, cc } => {
            let what = midi_cc_name(*cc);
            match p.channels.iter().find(|c| c.device.id == *device) {
                Some(ch) => (format!("{} · {what}", ch.name), col(ch.color)),
                None => (format!("Missing · {what}"), Color::hex(0x95A2B0)),
            }
        }
    }
}

/// A short name for a MIDI controller number.
pub fn midi_cc_name(cc: u8) -> String {
    match cc {
        1 => "Mod Wheel (CC1)".into(),
        2 => "Breath (CC2)".into(),
        7 => "Volume (CC7)".into(),
        10 => "Pan (CC10)".into(),
        11 => "Expression (CC11)".into(),
        64 => "Sustain Pedal (CC64)".into(),
        66 => "Sostenuto (CC66)".into(),
        67 => "Soft Pedal (CC67)".into(),
        _ => format!("MIDI CC{cc}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_automation_clip(
    ui: &mut Ui,
    v: &View,
    c: &Clip,
    cr: Rect,
    target: &peel_document::AutomationTarget,
    points: &[peel_document::AutomationPoint],
    sel: bool,
    hovered: bool,
    muted: bool,
    s: &Session,
) {
    let t = ui.theme.clone();
    let (label, color) = automation_label(s, target);
    ui.scene.rect(
        cr,
        CLIP_RADIUS,
        t.bg.mix(color, if muted { 0.06 } else { 0.14 }),
    );
    let header = Rect {
        h: 16.0f32.min(cr.h * 0.4),
        ..cr
    };
    ui.scene.rect_ex(
        header,
        [CLIP_RADIUS, CLIP_RADIUS, 0.0, 0.0],
        t.bg.mix(color, if muted { 0.25 } else { 0.5 }),
        0.0,
        Color::TRANSPARENT,
    );
    if cr.w > 26.0 {
        ui.with_clip(header, |ui| {
            let mut hr = header.shrink2(6.0, 0.0);
            ui.icon(hr.cut_left(12.0), icons::WAVE_SINE, 11.0, t.text);
            hr.cut_left(4.0);
            ui.text(
                hr,
                &label,
                t.caption.weight(peel_ui::FontWeight::SemiBold),
                t.text,
                Align::Start,
            );
        });
    }
    let body = auto_body(cr);
    ui.scene.push_clip(cr);
    ui.scene
        .hline(cr.x, cr.right(), body.center().y, t.grid_step);
    if !points.is_empty() {
        let line = color.lighten(0.25).alpha(if muted { 0.4 } else { 1.0 });
        let mut pts: Vec<peel_ui::Vec2> = Vec::with_capacity(points.len() * 2);
        for (i, pt) in points.iter().enumerate() {
            let (x, y) = auto_point_xy(v, c, body, pt);
            pts.push(vec2(x, y));
            if let Some(next) = points.get(i + 1)
                && pt.tension != 0.0
            {
                let (x1, y1) = auto_point_xy(v, c, body, next);
                let n = (((x1 - x) / 4.0) as usize).clamp(2, 48);
                for k in 1..n {
                    let f = k as f32 / n as f32;
                    let e = peel_document::automation_curve(f, pt.tension);
                    pts.push(vec2(x + (x1 - x) * f, y + (y1 - y) * e));
                }
            }
        }
        // Hold the first and last values to the clip edges.
        if let (Some(f), Some(l)) = (pts.first().copied(), pts.last().copied()) {
            pts.insert(0, vec2(cr.x, f.y));
            pts.push(vec2(cr.right(), l.y));
        }
        for w in pts.windows(2) {
            let (a, b) = (w[0], w[1]);
            let top = a.y.min(b.y);
            let _ = top;
            ui.scene.rect(
                Rect {
                    x: a.x,
                    y: a.y.max(b.y),
                    w: (b.x - a.x).max(0.0),
                    h: body.bottom() - a.y.max(b.y),
                },
                0.0,
                color.alpha(0.10),
            );
        }
        ui.scene.polyline(&pts, 1.75, line);
        if hovered || sel {
            for i in 0..points.len().saturating_sub(1) {
                if let Some(h) = tension_handle(v, c, body, points, i) {
                    ui.scene.circle_stroke(h, 3.0, t.bg, 1.25, line.alpha(0.8));
                }
            }
        }
        for pt in points {
            let (x, y) = auto_point_xy(v, c, body, pt);
            let r = if hovered || sel { 4.0 } else { 3.0 };
            ui.scene.circle_stroke(vec2(x, y), r, line, 1.5, t.bg);
        }
    }
    ui.scene.pop_clip();
    if sel {
        ui.scene
            .outline(cr, t.radius_s, 1.5, Color::WHITE.alpha(0.85));
    }
}

/// Where the tension handle of the segment after point `i` sits: halfway
/// along, on the curve. Flat segments have none.
fn tension_handle(
    v: &View,
    c: &Clip,
    body: Rect,
    points: &[peel_document::AutomationPoint],
    i: usize,
) -> Option<peel_ui::Vec2> {
    let (a, b) = (points.get(i)?, points.get(i + 1)?);
    if a.value == b.value {
        return None;
    }
    let (x0, y0) = auto_point_xy(v, c, body, a);
    let (x1, y1) = auto_point_xy(v, c, body, b);
    if x1 - x0 < 14.0 {
        return None;
    }
    let e = peel_document::automation_curve(0.5, a.tension);
    Some(vec2((x0 + x1) * 0.5, y0 + (y1 - y0) * e))
}

/// Adds an automation clip for `target` on the first empty lane (or a new
/// one), spanning the arrangement, flat at `value`. Returns the clip id.
pub fn create_automation(
    s: &mut Session,
    ws: &mut Workspace,
    target: peel_document::AutomationTarget,
    value: f32,
) -> Id {
    let arr = s.project().arrangement().clone();
    let bar = s.project().time_signature.bar_length();
    let length = arr.end().ceil_to(bar).max(bar.mul_int(8));
    let free_lane = arr
        .tracks
        .iter()
        .find(|t| !arr.clips.iter().any(|c| c.track == t.id))
        .map(|t| t.id);
    let (label, _) = automation_label(s, &target);
    let clip_id = Id::new();
    let value = value.clamp(0.0, 1.0);
    s.doc.edit(format!("Automate {label}"), |p| {
        let lane = match free_lane {
            Some(l) => l,
            None => {
                let t = Track {
                    id: Id::new(),
                    name: "Automation".into(),
                    color: None,
                    muted: false,
                };
                let id = t.id;
                p.arrangement_mut().tracks.push(t);
                id
            }
        };
        p.arrangement_mut().clips.push(Clip {
            id: clip_id,
            track: lane,
            start: Time::ZERO,
            length,
            offset: Time::ZERO,
            muted: false,
            content: ClipContent::Automation {
                target,
                points: vec![
                    peel_document::AutomationPoint::new(Time::ZERO, value),
                    peel_document::AutomationPoint::new(length, value),
                ],
            },
        });
    });
    ws.playlist.selection.clear();
    ws.playlist.selection.insert(clip_id);
    s.notify(
        format!("Created automation for {label}. Click the clip to add points."),
        false,
    );
    clip_id
}

/// Splits a clip at `at` into two clips that play exactly what it did.
pub fn slice_clip(s: &mut Session, c: &Clip, at: Time) -> Option<Id> {
    if at <= c.start || at >= c.end() {
        return None;
    }
    let cut = at - c.start;
    let mut left = c.clone();
    left.length = cut;
    let mut right = c.clone();
    right.id = Id::new();
    right.start = at;
    right.offset = c.offset + cut;
    right.length = c.length - cut;
    if let (ClipContent::Audio { fade_out, .. }, ClipContent::Audio { fade_in, .. }) =
        (&mut left.content, &mut right.content)
    {
        *fade_out = Time::ZERO;
        *fade_in = Time::ZERO;
    }
    if let ClipContent::Audio {
        fade_in, fade_out, ..
    } = &mut left.content
    {
        *fade_in = (*fade_in).min(left.length);
        *fade_out = (*fade_out).min(left.length);
    }
    if let ClipContent::Audio {
        fade_in, fade_out, ..
    } = &mut right.content
    {
        *fade_in = (*fade_in).min(right.length);
        *fade_out = (*fade_out).min(right.length);
    }
    let right_id = right.id;
    s.doc.edit("Slice clip", |p| {
        let arr = p.arrangement_mut();
        if let Some(x) = arr.clips.iter_mut().find(|x| x.id == left.id) {
            *x = left;
        }
        arr.clips.push(right);
    });
    Some(right_id)
}

fn arrangement_menu(ui: &mut Ui, anchor: Rect, s: &mut Session) {
    let id = W::new("arrangement-menu");
    let list: Vec<(Id, String)> = s
        .project()
        .arrangements
        .iter()
        .map(|a| (a.id, a.name.clone()))
        .collect();
    let active = s.project().active_arrangement;
    let mut items: Vec<MenuItem> = list
        .iter()
        .map(|(aid, n)| MenuItem::new(n.clone()).checked(*aid == active))
        .collect();
    items.push(MenuItem::Separator);
    let base = items.len();
    items.push(MenuItem::new("New arrangement"));
    items.push(MenuItem::new("Duplicate arrangement"));
    items.push(MenuItem::new("Rename arrangement"));
    items.push(MenuItem::new("Delete arrangement").enabled(list.len() > 1));
    if let Some(i) = ui.menu(id, vec2(anchor.x, anchor.bottom() + 4.0), &items) {
        if i < list.len() {
            let target = list[i].0;
            s.doc
                .edit("Switch arrangement", |p| p.active_arrangement = target);
        } else if i == base {
            let n = list.len() + 1;
            let arr = peel_document::Arrangement {
                id: Id::new(),
                name: format!("Arrangement {n}"),
                tracks: (1..=8)
                    .map(|k| Track {
                        id: Id::new(),
                        name: format!("Track {k}"),
                        color: None,
                        muted: false,
                    })
                    .collect(),
                clips: Vec::new(),
                markers: Vec::new(),
                loop_range: None,
            };
            let aid = arr.id;
            s.doc.edit("New arrangement", |p| {
                p.arrangements.push(std::sync::Arc::new(arr));
                p.active_arrangement = aid;
            });
        } else if i == base + 1 {
            // Copies share patterns and media; lanes and clips get new ids.
            let src = s.project().arrangement().as_ref().clone();
            let mut lane_map = HashMap::new();
            let tracks: Vec<Track> = src
                .tracks
                .iter()
                .map(|t| {
                    let nid = Id::new();
                    lane_map.insert(t.id, nid);
                    Track {
                        id: nid,
                        ..t.clone()
                    }
                })
                .collect();
            let clips: Vec<Clip> = src
                .clips
                .iter()
                .map(|c| Clip {
                    id: Id::new(),
                    track: lane_map.get(&c.track).copied().unwrap_or(c.track),
                    ..c.clone()
                })
                .collect();
            let arr = peel_document::Arrangement {
                id: Id::new(),
                name: format!("{} copy", src.name),
                tracks,
                clips,
                markers: src.markers.clone(),
                loop_range: src.loop_range,
            };
            let aid = arr.id;
            s.doc.edit("Duplicate arrangement", |p| {
                p.arrangements.push(std::sync::Arc::new(arr));
                p.active_arrangement = aid;
            });
        } else if i == base + 2 {
            let name = s.project().arrangement().name.clone();
            crate::rename::open(ui, W::new("arrangement-rename"), anchor, &name);
        } else if i == base + 3 {
            s.doc.edit("Delete arrangement", |p| {
                let gone = p.active_arrangement;
                p.arrangements.retain(|a| a.id != gone);
                p.active_arrangement = p.arrangements[0].id;
            });
        }
    }
    if let Some(name) = crate::rename::show(ui, W::new("arrangement-rename")) {
        s.doc
            .edit("Rename arrangement", |p| p.arrangement_mut().name = name);
    }
}

/// The take being recorded, growing from where it started to the playhead
/// on every armed lane.
fn take_preview(ui: &mut Ui, v: &View, tracks: &[Track], s: &Session) {
    let st = W::new("take-start");
    let status = s.recording_status().filter(|r| !r.waiting_for_transport);
    let Some(status) = status else {
        *ui.state::<Option<f64>>(st) = None;
        return;
    };
    let now = s.position();
    let start = *ui.state::<Option<f64>>(st).get_or_insert(now);
    if now <= start {
        return;
    }
    let t = ui.theme.clone();
    ui.scene.push_clip(v.lanes);
    for lane in &status.lanes {
        let Some(i) = tracks.iter().position(|x| x.id == *lane) else {
            continue;
        };
        let (x0, x1) = (v.x(start), v.x(now));
        let r = Rect {
            x: x0,
            y: v.lane_y(i) + 2.0,
            w: x1 - x0,
            h: v.lane_h - 4.0,
        };
        ui.scene.rect(r, t.radius_s, t.record.alpha(0.18));
        ui.scene.outline(r, t.radius_s, 1.0, t.record.alpha(0.7));
        ui.scene
            .rect(Rect { h: 18.0, ..r }, t.radius_s, t.record.alpha(0.85));
        ui.text(
            Rect {
                x: r.x + 6.0,
                w: (r.w - 6.0).max(0.0),
                h: 18.0,
                ..r
            },
            "Recording",
            t.caption,
            ink_on(t.record),
            Align::Start,
        );
    }
    ui.scene.pop_clip();
}

/// Keeps tempo segments sorted, the first at zero, and every ramp ending
/// at the next segment's tempo so the curve is continuous.
pub(crate) fn tidy_tempo(segs: &mut [peel_time::TempoSegment]) {
    segs.sort_by_key(|x| x.start);
    if let Some(first) = segs.first_mut() {
        first.start = Time::ZERO;
    }
    for i in 0..segs.len() {
        let next = segs.get(i + 1).map(|n| n.bpm);
        if segs[i].ramp_to.is_some() {
            segs[i].ramp_to = next;
        }
    }
}

/// Tempo strip: the tempo curve with a handle at every change. Drag a
/// handle to change its tempo (and position), double-click to add a
/// change, right-click a handle to ramp to the next change or delete it.
fn tempo_lane(ui: &mut Ui, head: Rect, r: Rect, v: &View, s: &mut Session) {
    let t = ui.theme.clone();
    let mut segs = s.project().tempo.clone();
    tidy_tempo(&mut segs);
    let map = peel_time::TempoMap::from_segments(&segs);

    // Head.
    let hrow = Rect {
        w: head.w - 6.0,
        ..head.shrink2(0.0, 2.0)
    };
    let mut hi = hrow.shrink2(12.0, 8.0);
    ui.text(hi.cut_top(20.0), "Tempo", t.label, t.text, Align::Start);
    let now = map.bpm_at(s.position().max(0.0));
    ui.text(
        hi,
        &format!("{now:.1} BPM"),
        t.caption,
        t.text_faint,
        Align::Start,
    );

    // Range shown: the tempos used, with room above and below.
    let used = segs
        .iter()
        .flat_map(|x| [x.bpm, x.ramp_to.unwrap_or(x.bpm)]);
    let (mut lo, mut top) = used.fold((f64::MAX, f64::MIN), |(a, b), x| (a.min(x), b.max(x)));
    lo = ((lo - 10.0) / 10.0).floor() * 10.0;
    top = ((top + 10.0) / 10.0).ceil() * 10.0;
    if top - lo < 40.0 {
        let mid = (top + lo) * 0.5;
        (lo, top) = (mid - 20.0, mid + 20.0);
    }
    // Held still while a handle is dragged, so the scale does not move
    // under the pointer.
    let frozen = W::new("tempo-range");
    if let Some((a, b)) = *ui.state::<Option<(f64, f64)>>(frozen) {
        (lo, top) = (a, b);
    }
    let lane = r.shrink2(0.0, 10.0);
    let y_of = |bpm: f64| lane.bottom() - ((bpm - lo) / (top - lo)) as f32 * lane.h;
    let bpm_of = |y: f32| lo + ((lane.bottom() - y) / lane.h).clamp(0.0, 1.0) as f64 * (top - lo);

    ui.scene.push_clip(r);
    ui.scene.rect(r, 0.0, t.bg.alpha(0.35));
    ui.scene.hline(r.x, r.right(), r.bottom() - 0.5, t.line);
    // Curve.
    for (i, seg) in segs.iter().enumerate() {
        let x0 = v.x(seg.start.as_f64());
        let x1 = segs
            .get(i + 1)
            .map_or(r.right() + 4.0, |n| v.x(n.start.as_f64()));
        let y0 = y_of(seg.bpm);
        let end = if segs.get(i + 1).is_some() {
            seg.ramp_to.unwrap_or(seg.bpm)
        } else {
            seg.bpm
        };
        let y1 = y_of(end);
        ui.scene.line(vec2(x0, y0), vec2(x1, y1), 2.0, t.accent);
        if let Some(n) = segs.get(i + 1) {
            ui.scene
                .line(vec2(x1, y1), vec2(x1, y_of(n.bpm)), 2.0, t.accent);
        }
    }
    ui.scene.pop_clip();

    // Double-click empty space: add a change.
    let bg = ui.interact(W::new("tempo-lane"), r);
    if bg.double_clicked
        && let Some(p) = ui.pointer()
    {
        let at = Time::from_f64(v.beat(p.x).max(0.0), 960).round_to(Time::beats(1));
        if at > Time::ZERO && segs.iter().all(|x| x.start != at) {
            let bpm = map.bpm_at(at.as_f64()).round();
            s.doc.edit("Add tempo change", |p| {
                p.tempo.push(peel_time::TempoSegment {
                    start: at,
                    bpm,
                    ramp_to: None,
                });
                tidy_tempo(&mut p.tempo);
            });
            return;
        }
    }
    ui.tooltip(&bg, "Double-click to add a tempo change");

    for (i, seg) in segs.iter().enumerate() {
        let c = vec2(v.x(seg.start.as_f64()), y_of(seg.bpm));
        if c.x < r.x - 8.0 || c.x > r.right() + 8.0 {
            continue;
        }
        let id = W::new(("tempo-pt", i));
        let hit = Rect::from_center(c, 14.0, 14.0);
        let resp = ui.interact_button(id, hit, &format!("Tempo {:.1} BPM", seg.bpm));
        let hot = resp.hovered || resp.active;
        ui.scene.circle_stroke(
            c,
            if hot { 5.5 } else { 4.5 },
            t.panel,
            2.0,
            if hot { t.text } else { t.accent },
        );
        let label = format!("{:.0}", seg.bpm);
        let lx = (c.x + 8.0).max(r.x + 2.0);
        ui.text_at(
            vec2(lx, (c.y - 18.0).clamp(r.y, r.bottom() - 14.0)),
            &label,
            t.caption,
            t.text_dim,
        );
        if hot {
            ui.set_cursor(CursorIcon::Grab);
        }
        if resp.pressed {
            *ui.state::<Option<(f64, f64)>>(frozen) = Some((lo, top));
        }
        if resp.dragging
            && let Some(p) = ui.pointer()
        {
            let fine = ui.input.modifiers.shift;
            let raw = bpm_of(p.y);
            let bpm = if fine {
                (raw * 10.0).round() / 10.0
            } else {
                raw.round()
            }
            .clamp(20.0, 400.0);
            let at = if i == 0 {
                Time::ZERO
            } else {
                let lo_t = segs[i - 1].start + Time::beats(1);
                let hi_t = segs.get(i + 1).map(|n| n.start - Time::beats(1));
                let mut at = Time::from_f64(v.beat(p.x).max(0.0), 960)
                    .round_to(Time::beats(1))
                    .max(lo_t);
                if let Some(h) = hi_t {
                    at = at.min(h);
                }
                at
            };
            if bpm != seg.bpm || at != seg.start {
                s.doc.edit_gesture(id.0, "Change tempo", |p| {
                    tidy_tempo(&mut p.tempo);
                    if let Some(x) = p.tempo.get_mut(i) {
                        x.bpm = bpm;
                        x.start = at;
                    }
                    tidy_tempo(&mut p.tempo);
                });
            }
        }
        if resp.released {
            *ui.state::<Option<(f64, f64)>>(frozen) = None;
            s.doc.end_gesture();
        }
        ui.tooltip(&resp, &format!("{:.1} BPM", seg.bpm));
        let menu = id.with("menu");
        if resp.secondary_clicked {
            ui.open_popup(menu, hit);
        }
        let has_next = i + 1 < segs.len();
        let items = [
            MenuItem::new("Ramp to next")
                .checked(seg.ramp_to.is_some())
                .enabled(has_next),
            MenuItem::new("Delete").enabled(i > 0),
        ];
        match ui.menu(menu, vec2(hit.x, hit.bottom() + 4.0), &items) {
            Some(0) => {
                let on = seg.ramp_to.is_none();
                s.doc
                    .edit(if on { "Ramp tempo" } else { "Step tempo" }, |p| {
                        tidy_tempo(&mut p.tempo);
                        if let Some(x) = p.tempo.get_mut(i) {
                            x.ramp_to = on.then_some(x.bpm);
                        }
                        tidy_tempo(&mut p.tempo);
                    });
            }
            Some(_) => {
                s.doc.edit("Delete tempo change", |p| {
                    tidy_tempo(&mut p.tempo);
                    if i > 0 && i < p.tempo.len() {
                        p.tempo.remove(i);
                    }
                    tidy_tempo(&mut p.tempo);
                });
            }
            None => {}
        }
    }
}

/// Screen position of a tension handle, for tests.
#[cfg(test)]
pub(crate) fn tension_handle_at(
    canvas: Rect,
    ws: &Workspace,
    s: &Session,
    clip: Id,
    i: usize,
) -> Option<peel_ui::Vec2> {
    let v = View {
        lanes: canvas,
        zoom_x: ws.playlist.zoom_x,
        scroll: ws.playlist.scroll_beats,
        lane_h: ws.playlist.lane_h,
        scroll_y: ws.playlist.scroll_y,
    };
    let arr = s.project().arrangement();
    let c = arr.clips.iter().find(|c| c.id == clip)?;
    let lane = arr.tracks.iter().position(|t| t.id == c.track)?;
    let ClipContent::Automation { points, .. } = &c.content else {
        return None;
    };
    tension_handle(&v, c, auto_body(clip_rect(&v, lane, c)), points, i)
}

/// Cuts an audio clip at every hit (onset) inside it, as one undo step.
/// Returns how many clips it became, or `None` when there was nothing to
/// cut (no decoded audio or no hits).
pub fn chop_at_hits(s: &mut Session, c: &Clip) -> Option<usize> {
    let ClipContent::Audio {
        asset, source_bpm, ..
    } = &c.content
    else {
        return None;
    };
    let buf = s.sample(*asset)?;
    let rate = f64::from(buf.sample_rate);
    let hits = peel_dsp::stretch::onsets(&buf.channels, buf.frames(), buf.sample_rate);
    // File seconds to song beats for this clip.
    let tempo = peel_time::TempoMap::from_segments(&s.project().tempo);
    let src_start = match source_bpm {
        Some(b) => c.offset.as_f64() * 60.0 / b,
        None => peel_session::clip_source_start_seconds(&tempo, c),
    };
    let beat_of = |sec: f64| -> f64 {
        match source_bpm {
            Some(b) => c.start.as_f64() + (sec - src_start) * b / 60.0,
            None => {
                let t0 = tempo.beat_to_seconds(c.start.as_f64());
                tempo.seconds_to_beat(t0 + sec - src_start)
            }
        }
    };
    let min = Time::new(1, 32);
    let mut cuts: Vec<Time> = hits
        .iter()
        .map(|&f| Time::from_f64(beat_of(f as f64 / rate), 960))
        .filter(|t| *t > c.start + min && *t < c.end() - min)
        .collect();
    cuts.dedup();
    if cuts.is_empty() {
        return None;
    }
    let mut bounds = vec![c.start];
    bounds.extend(cuts);
    bounds.push(c.end());
    let pieces: Vec<Clip> = bounds
        .windows(2)
        .enumerate()
        .map(|(i, w)| {
            let mut p = c.clone();
            if i > 0 {
                p.id = Id::new();
            }
            p.start = w[0];
            p.length = w[1] - w[0];
            p.offset = c.offset + (w[0] - c.start);
            if let ClipContent::Audio {
                fade_in, fade_out, ..
            } = &mut p.content
            {
                *fade_in = Time::ZERO;
                *fade_out = Time::ZERO;
            }
            p
        })
        .collect();
    let n = pieces.len();
    let cid = c.id;
    s.doc.edit("Chop at hits", |p| {
        let arr = p.arrangement_mut();
        arr.clips.retain(|x| x.id != cid);
        arr.clips.extend(pieces);
    });
    Some(n)
}
