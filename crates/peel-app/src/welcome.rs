//! Start screen: a blank project, a template, something recent, or
//! unsaved work recovered after a crash.

use std::path::PathBuf;

use peel_ui::{Align, CursorIcon, Id as W, LayerId, Rect, Ui, icons, vec2};

/// What the user picked.
pub enum WelcomeAction {
    /// A blank project.
    New,
    /// Start from `peel_session::template::templates()[i]`.
    Template(usize),
    /// Start from one of the user's own templates (its root file).
    UserTemplate(PathBuf),
    /// Open `peel_session::template::demos()[i]`.
    Demo(usize),
    Open,
    OpenPath(PathBuf),
    Recover(PathBuf),
    Close,
}

pub struct WelcomeData<'a> {
    pub recent: &'a [PathBuf],
    pub recovered: &'a [peel_project::AutosaveEntry],
    pub templates: &'a crate::user_templates::Library,
    pub show_at_startup: &'a mut bool,
}

/// Height of a template button with the gap under it.
const TEMPLATE_ROW: f32 = 34.0;
/// Height of a demo song row with the gap under it.
const DEMO_ROW: f32 = 56.0;

fn big_button(ui: &mut Ui, id: W, r: Rect, icon: &str, label: &str, primary: bool) -> bool {
    let t = ui.theme.clone();
    let resp = ui.interact_button(id, r, label);
    let hot = ui.animate_bool(id, resp.hovered);
    let (fill, ink) = if primary {
        (t.accent.lighten(hot * 0.08), t.text_on_accent)
    } else {
        (t.control.mix(t.control_hover, hot), t.text)
    };
    ui.keycap(r, t.radius_m, fill, resp.active);
    let mut inner = r.shrink2(14.0, 0.0);
    ui.icon(inner.cut_left(20.0), icon, 17.0, ink);
    inner.cut_left(12.0);
    ui.text(inner, label, t.label.size(14.0), ink, Align::Start);
    if resp.hovered {
        ui.set_cursor(CursorIcon::Pointer);
    }
    ui.focus_ring(id, r, t.radius_l);
    resp.clicked
}

/// One demo song: a play tile in a step-key color, its title, and its
/// style, tempo and key.
fn demo_row(ui: &mut Ui, id: W, r: Rect, n: usize, demo: &peel_session::template::Demo) -> bool {
    let t = ui.theme.clone();
    let resp = ui.interact_button(id, r, &format!("Open {}", demo.title));
    let hot = ui.animate_bool(id, resp.hovered);
    if hot > 0.0 {
        ui.scene.rect(r, t.radius_m, t.raised.alpha(hot));
    }
    let key = t.step_keys[n % t.step_keys.len()];
    let mut inner = r.shrink2(8.0, 0.0);
    let tile = Rect::from_center(inner.cut_left(36.0).center(), 36.0, 36.0);
    ui.keycap(tile, t.radius_m, key.lighten(hot * 0.08), false);
    ui.icon_fill(tile, icons::PLAY, 15.0, crate::util::ink_on(key));
    inner.cut_left(12.0);
    let mut text = Rect::from_center(inner.center(), inner.w, 36.0);
    let title = ui.fit_text(demo.title, t.label.size(14.0), text.w);
    ui.text(
        text.cut_top(19.0),
        &title,
        t.label.size(14.0),
        t.text,
        Align::Start,
    );
    let sub = format!("{} · {:.0} BPM · {}", demo.style, demo.bpm, demo.key);
    let sub = ui.fit_text(&sub, t.caption, text.w);
    ui.text(text, &sub, t.caption, t.text_faint, Align::Start);
    if resp.hovered {
        ui.set_cursor(CursorIcon::Pointer);
    }
    ui.focus_ring(id, r, t.radius_m);
    resp.clicked
}

pub fn draw(ui: &mut Ui, data: WelcomeData) -> Option<WelcomeAction> {
    let t = ui.theme.clone();
    let screen = ui.screen();
    let mut action = None;
    ui.with_layer(LayerId::POPUP, |ui| {
        ui.scene.push_clip_absolute(Rect::EVERYTHING);
        let backdrop = ui.interact(W::new("welcome-backdrop"), screen);
        ui.scene.rect(screen, 0.0, t.bg.alpha(0.82));
        let styles = crate::project::styles();
        let demos = peel_session::template::demos();
        let has_mine = !data.templates.is_empty();
        let cols = 2usize;
        let rows = styles.len().div_ceil(cols);
        // Three columns: start something, hear something, pick up where
        // you left off. The tallest column sets the height.
        let start_h = 56.0
            + 24.0
            + 96.0
            + 22.0
            + 26.0
            + rows as f32 * TEMPLATE_ROW
            + if has_mine { 34.0 } else { 0.0 }
            + 38.0;
        let songs_h = 26.0 + demos.len() as f32 * DEMO_ROW;
        let w = 1000.0f32.min(screen.w - 40.0);
        let h = (start_h.max(songs_h) + 64.0)
            .max(480.0)
            .min(screen.h - 40.0);
        let panel = Rect::from_center(screen.center(), w, h);
        ui.interact(W::new("welcome-panel"), panel);
        ui.scene
            .shadow(panel.translate(vec2(0.0, 8.0)), t.radius_l, 24.0, t.shadow);
        ui.scene.rect(panel, t.radius_l, t.panel);
        if backdrop.clicked {
            action = Some(WelcomeAction::Close);
        }

        let mut body = panel.shrink(32.0);
        let mut left = body.cut_left(264.0);
        body.cut_left(32.0);
        let right = body.cut_right(280.0f32.min(body.w * 0.45));
        body.cut_right(32.0);
        let mut songs = body;
        for x in [songs.x - 16.0, right.x - 16.0] {
            ui.scene
                .vline(x, panel.y + 32.0, panel.bottom() - 32.0, t.line);
        }

        // Brand.
        let brand = left.cut_top(56.0);
        crate::util::logo(ui, vec2(brand.x + 22.0, brand.center().y), 20.0, t.panel);
        let mut tr = brand;
        tr.cut_left(56.0);
        tr.cut_top(4.0);
        ui.text(
            tr.cut_top(30.0),
            "Peachey Studio",
            t.heading.size(22.0),
            t.text,
            Align::Start,
        );
        ui.text(
            tr.cut_top(16.0),
            &format!("Version {}", env!("CARGO_PKG_VERSION")),
            t.caption,
            t.text_faint,
            Align::Start,
        );
        left.cut_top(24.0);

        let buttons = [
            (icons::PLUS, "New project", true, 0usize),
            (icons::FOLDER_OPEN, "Open project or MIDI", false, 1),
        ];
        for (icon, label, primary, which) in buttons {
            let r = left.cut_top(44.0);
            left.cut_top(4.0);
            if big_button(ui, W::new(("welcome-btn", which)), r, icon, label, primary) {
                action = Some(if which == 0 {
                    WelcomeAction::New
                } else {
                    WelcomeAction::Open
                });
            }
        }
        // Startup toggle, pinned to the bottom of the column.
        let toggle_r = left.cut_bottom(26.0);
        let mut tr = toggle_r;
        let led = ui.led(
            W::new("welcome-startup"),
            tr.cut_left(22.0),
            *data.show_at_startup,
            t.play,
        );
        if led.clicked {
            *data.show_at_startup = !*data.show_at_startup;
        }
        tr.cut_left(8.0);
        ui.text(
            tr,
            "Show this at startup",
            t.caption,
            t.text_dim,
            Align::Start,
        );
        left.cut_bottom(12.0);

        // Templates: two columns, as many rows as the catalog needs and
        // the window allows (all of them are in File > New from template).
        left.cut_top(22.0);
        ui.text(
            left.cut_top(22.0),
            "Templates",
            t.label,
            t.text_dim,
            Align::Start,
        );
        left.cut_top(4.0);
        let foot = if has_mine { 34.0 } else { 0.0 };
        let fit_rows = (((left.h - foot) / TEMPLATE_ROW).floor().max(1.0)) as usize;
        let cell_w = (left.w - 4.0 * (cols - 1) as f32) / cols as f32;
        for (n, (i, name)) in styles.iter().enumerate().take(fit_rows * cols) {
            let r = Rect {
                x: left.x + (n % cols) as f32 * (cell_w + 4.0),
                y: left.y + (n / cols) as f32 * TEMPLATE_ROW,
                w: cell_w,
                h: TEMPLATE_ROW - 4.0,
            };
            let label = ui.fit_text(name, t.label, r.w - 16.0);
            if ui
                .button(W::new(("welcome-template", *i)), r, &label)
                .clicked
            {
                action = Some(WelcomeAction::Template(*i));
            }
        }
        left.cut_top(rows.min(fit_rows) as f32 * TEMPLATE_ROW);
        if has_mine {
            left.cut_top(4.0);
            let row = left.cut_top(30.0);
            let label = "My templates";
            let mw = ui.quiet_button_width(true, label, true);
            let r = row.with_w(mw.min(row.w));
            let list = crate::user_templates::picker_id();
            let open = ui.is_popup_open(list);
            let resp = ui.quiet_button(
                W::new("welcome-my-templates"),
                r,
                Some(icons::FOLDER_OPEN),
                label,
                true,
                open,
            );
            if resp.clicked && !open {
                crate::user_templates::open_picker(ui, r);
            }
            if let Some(path) = crate::user_templates::picker(ui, r, data.templates) {
                action = Some(WelcomeAction::UserTemplate(path));
            }
        }

        // Demo songs: finished arrangements to open, play and take apart.
        ui.text(
            songs.cut_top(22.0),
            "Demo songs",
            t.label,
            t.text_dim,
            Align::Start,
        );
        songs.cut_top(4.0);
        for (i, demo) in demos.iter().enumerate() {
            if songs.h < DEMO_ROW - 4.0 {
                break;
            }
            let mut r = songs.cut_top(DEMO_ROW);
            r.h -= 4.0;
            r.x -= 8.0;
            r.w += 8.0;
            let id = if i == 0 {
                W::new("welcome-demo")
            } else {
                W::new(("welcome-demo", i))
            };
            if demo_row(ui, id, r, i, demo) {
                action = Some(WelcomeAction::Demo(i));
            }
        }

        // Recent and recovered projects.
        let mut list = right;
        let mut y_rows = Vec::new();
        if !data.recovered.is_empty() {
            ui.text(
                list.cut_top(22.0),
                "Recovered after an unexpected exit",
                t.label,
                t.record,
                Align::Start,
            );
            for (i, _) in data.recovered.iter().take(3).enumerate() {
                y_rows.push((list.cut_top(44.0), true, i));
                list.cut_top(4.0);
            }
            list.cut_top(12.0);
        }
        ui.text(
            list.cut_top(22.0),
            "Recent",
            t.label,
            t.text_dim,
            Align::Start,
        );
        list.cut_top(4.0);
        if data.recent.is_empty() {
            let mut hint = list.cut_top(40.0);
            hint.cut_top(6.0);
            ui.text(
                hint.cut_top(20.0),
                "Projects you save show up here.",
                t.body,
                t.text_faint,
                Align::Start,
            );
        }
        for (i, _) in data.recent.iter().enumerate() {
            if list.h < 44.0 {
                break;
            }
            y_rows.push((list.cut_top(44.0), false, i));
            list.cut_top(4.0);
        }
        for (row, recovered, i) in y_rows {
            let id = W::new(("welcome-row", recovered, i));
            // The hover fill reaches past the text so names line up with
            // the heading above them.
            let row = Rect {
                x: row.x - 8.0,
                w: row.w + 8.0,
                ..row
            };
            let resp = ui.interact(id, row);
            let hot = ui.animate_bool(id, resp.hovered);
            if hot > 0.0 {
                ui.scene.rect(row, t.radius_m, t.raised.alpha(hot));
            }
            let mut inner = row.shrink2(8.0, 6.0);
            let icon_r = inner.cut_left(20.0);
            inner.cut_left(10.0);
            let (title, sub, path) = if recovered {
                let e = &data.recovered[i];
                (
                    e.title.clone(),
                    format!(
                        "Autosaved {}",
                        e.saved_at
                            .replace('T', " ")
                            .chars()
                            .take(16)
                            .collect::<String>()
                    ),
                    e.path.clone(),
                )
            } else {
                let p = &data.recent[i];
                let title = p
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let sub = p
                    .parent()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default();
                (title, sub, p.clone())
            };
            ui.icon(
                icon_r,
                if recovered {
                    icons::LIGHTNING
                } else {
                    icons::MUSIC_NOTE
                },
                15.0,
                if recovered { t.record } else { t.accent },
            );
            let title = ui.fit_text(&title, t.label, inner.w);
            ui.text(inner.cut_top(18.0), &title, t.label, t.text, Align::Start);
            let sub = ui.fit_text(&sub, t.caption, inner.w);
            ui.text(inner, &sub, t.caption, t.text_faint, Align::Start);
            if resp.hovered {
                ui.set_cursor(CursorIcon::Pointer);
            }
            if resp.clicked {
                action = Some(if recovered {
                    WelcomeAction::Recover(path)
                } else {
                    WelcomeAction::OpenPath(path)
                });
            }
        }
        ui.scene.pop_clip();
    });
    // Escape closes the template list first, then the start screen.
    if ui.input.take_named(peel_ui::NamedKey::Escape) {
        if ui.is_popup_open(crate::user_templates::picker_id()) {
            ui.close_popup();
        } else {
            action = Some(WelcomeAction::Close);
        }
    }
    action
}
