//! Application menus and the commands behind them.
//!
//! One [`MenuBar`] describes File, Edit, View, Transport and Help (plus the
//! application and Window menus on macOS). It becomes the platform menu bar
//! on macOS and the in-window strip elsewhere. Menus, their
//! shortcuts and the start screen all end in [`run`], so each action has
//! one implementation.

use peel_session::PlayMode;
use peel_ui::{
    Id as W, Menu, MenuBar, MenuCommand, MenuEntry, MenuRole, Rect, Shortcut, ShortcutKey,
    StandardItem, Ui,
};

use crate::project::Change;
use crate::studio::Studio;
use crate::workspace::DockTab;

/// The in-window strip's id.
pub const STRIP: W = W(0x5e1f_a11e_0b42);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    NewProject,
    Template(usize),
    /// `template::demos()[i]`.
    Demo(usize),
    StartScreen,
    Open,
    Recent(usize),
    ClearRecent,
    Close,
    Save,
    SaveAs,
    ExportAudio,
    ExportMidi,
    Settings,
    Quit,
    Undo,
    Redo,
    Browser,
    Inspector,
    Editor,
    Scripts,
    Dock(DockTab),
    TempoLane,
    MasterMeter,
    StatusDetails,
    ResetLayout,
    PlayStop,
    Record,
    ToStart,
    ToEnd,
    SongMode,
    LoopSong,
    Keys,
    Hints,
    SaveTemplate,
    /// A user template by its catalog slot (see `user_templates::Library`).
    UserTemplate(usize),
}

/// Demo songs after the first, which keeps its place among the simple
/// commands.
const DEMO_BASE: u32 = 800;
const DEMO_END: u32 = 900;
const TEMPLATE_BASE: u32 = 1000;
const RECENT_BASE: u32 = 2000;
const USER_TEMPLATE_BASE: u32 = 3000;
const USER_TEMPLATE_END: u32 = USER_TEMPLATE_BASE + crate::user_templates::MAX_TEMPLATES as u32;

// New commands go at the end so existing ids never move.
const SIMPLE: [Command; 30] = [
    Command::NewProject,
    Command::Demo(0),
    Command::StartScreen,
    Command::Open,
    Command::ClearRecent,
    Command::Close,
    Command::Save,
    Command::SaveAs,
    Command::ExportAudio,
    Command::ExportMidi,
    Command::Settings,
    Command::Quit,
    Command::Undo,
    Command::Redo,
    Command::Browser,
    Command::Inspector,
    Command::Editor,
    Command::Scripts,
    Command::TempoLane,
    Command::MasterMeter,
    Command::StatusDetails,
    Command::ResetLayout,
    Command::PlayStop,
    Command::Record,
    Command::ToStart,
    Command::ToEnd,
    Command::SongMode,
    Command::LoopSong,
    Command::Keys,
    Command::SaveTemplate,
];

const DOCKS: [DockTab; 4] = [
    DockTab::Rack,
    DockTab::PianoRoll,
    DockTab::Mixer,
    DockTab::Perform,
];

impl Command {
    pub fn id(self) -> u32 {
        match self {
            Command::Template(i) => TEMPLATE_BASE + i as u32,
            Command::Demo(i) if i > 0 => (DEMO_BASE + i as u32).min(DEMO_END - 1),
            Command::Recent(i) => RECENT_BASE + i as u32,
            Command::UserTemplate(i) => USER_TEMPLATE_BASE + i as u32,
            Command::Dock(tab) => 900 + DOCKS.iter().position(|d| *d == tab).unwrap_or(0) as u32,
            Command::Hints => 950,
            c => 1 + SIMPLE.iter().position(|s| *s == c).unwrap_or(0) as u32,
        }
    }

    pub fn from_id(id: u32) -> Option<Command> {
        match id {
            950 => Some(Command::Hints),
            DEMO_BASE..DEMO_END => Some(Command::Demo((id - DEMO_BASE) as usize)),
            900..=903 => Some(Command::Dock(DOCKS[(id - 900) as usize])),
            TEMPLATE_BASE..RECENT_BASE => Some(Command::Template((id - TEMPLATE_BASE) as usize)),
            RECENT_BASE..USER_TEMPLATE_BASE => Some(Command::Recent((id - RECENT_BASE) as usize)),
            USER_TEMPLATE_BASE..USER_TEMPLATE_END => {
                Some(Command::UserTemplate((id - USER_TEMPLATE_BASE) as usize))
            }
            1..=30 => SIMPLE.get(id as usize - 1).copied(),
            _ => None,
        }
    }

    /// Commands that work while the start screen shows.
    fn on_start_screen(self) -> bool {
        matches!(
            self,
            Command::NewProject
                | Command::Template(_)
                | Command::UserTemplate(_)
                | Command::Demo(_)
                | Command::StartScreen
                | Command::Open
                | Command::Recent(_)
                | Command::ClearRecent
                | Command::Settings
                | Command::Quit
                | Command::Keys
                | Command::Hints
        )
    }
}

fn cmd(c: Command, label: impl Into<String>) -> MenuCommand {
    MenuCommand::new(c.id(), label)
}

fn key(k: ShortcutKey) -> Shortcut {
    Shortcut::key(k)
}

/// The full menu bar. `mac` lays it out the macOS way (an application
/// menu holding About, Settings and Quit, plus a Window menu).
pub fn bar(studio: &Studio, mac: bool) -> MenuBar {
    let p = &studio.prefs;
    let s = &studio.session;
    let mut menus = Vec::new();

    if mac {
        let mut app = Menu::new("Peachey Studio").role(MenuRole::App);
        app.push(MenuEntry::Standard(StandardItem::About {
            name: "Peachey Studio".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }));
        app.separator();
        app.push(cmd(Command::Settings, "Settings…").shortcut(Shortcut::cmd(',')));
        app.separator();
        app.push(MenuEntry::Standard(StandardItem::Services));
        app.separator();
        app.push(MenuEntry::Standard(StandardItem::Hide));
        app.push(MenuEntry::Standard(StandardItem::HideOthers));
        app.push(MenuEntry::Standard(StandardItem::ShowAll));
        app.separator();
        app.push(cmd(Command::Quit, "Quit Peachey Studio").shortcut(Shortcut::cmd('q')));
        menus.push(app);
    }

    let mut file = Menu::new("File");
    file.push(cmd(Command::NewProject, "New project").shortcut(Shortcut::cmd('n')));
    let mut styles = Menu::new("New from template");
    for (i, name) in crate::project::styles() {
        styles.push(cmd(Command::Template(i), name));
    }
    let mut mine = Menu::new("My templates");
    for (slot, entry) in studio.templates.entries() {
        mine.push(cmd(Command::UserTemplate(slot), entry.name.clone()));
    }
    styles.separator();
    styles.push(MenuEntry::Submenu {
        enabled: !studio.templates.is_empty(),
        menu: mine,
        inline: None,
    });
    file.push(MenuEntry::Submenu {
        menu: styles,
        enabled: true,
        inline: Some(cmd(Command::StartScreen, "New from template…")),
    });
    let demos = peel_session::template::demos();
    let mut songs = Menu::new("Demo songs");
    for (i, demo) in demos.iter().enumerate() {
        songs.push(cmd(
            Command::Demo(i),
            format!("{} ({})", demo.title, demo.style),
        ));
    }
    file.push(MenuEntry::Submenu {
        menu: songs,
        enabled: !demos.is_empty(),
        inline: None,
    });
    file.separator();
    file.push(cmd(Command::Open, "Open…").shortcut(Shortcut::cmd('o')));
    let recent = p.existing_recent();
    let mut recent_menu = Menu::new("Open recent");
    for (i, path) in recent.iter().enumerate() {
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        recent_menu.push(cmd(Command::Recent(i), name));
    }
    if !recent.is_empty() {
        recent_menu.separator();
        recent_menu.push(cmd(Command::ClearRecent, "Clear recent"));
    }
    file.push(MenuEntry::Submenu {
        menu: recent_menu,
        enabled: !recent.is_empty(),
        inline: None,
    });
    file.separator();
    file.push(cmd(Command::Close, "Close project").shortcut(Shortcut::cmd('w')));
    file.push(cmd(Command::Save, "Save").shortcut(Shortcut::cmd('s')));
    file.push(cmd(Command::SaveAs, "Save as…").shortcut(Shortcut::cmd('s').shift()));
    file.push(cmd(Command::SaveTemplate, "Save as template…"));
    file.separator();
    file.push(cmd(Command::ExportAudio, "Export audio…").shortcut(Shortcut::cmd('e')));
    file.push(cmd(Command::ExportMidi, "Export MIDI…"));
    if !mac {
        file.separator();
        file.push(cmd(Command::Settings, "Settings…").shortcut(Shortcut::cmd(',')));
        file.separator();
        file.push(cmd(Command::Quit, "Quit").shortcut(Shortcut::cmd('q')));
    }
    menus.push(file);

    let mut edit = Menu::new("Edit");
    edit.push(
        cmd(
            Command::Undo,
            s.doc
                .undo_label()
                .map_or_else(|| "Undo".to_string(), |l| format!("Undo {l}")),
        )
        .shortcut(Shortcut::cmd('z'))
        .enabled(s.doc.undo_label().is_some()),
    );
    edit.push(
        cmd(
            Command::Redo,
            s.doc
                .redo_label()
                .map_or_else(|| "Redo".to_string(), |l| format!("Redo {l}")),
        )
        .shortcut(Shortcut::cmd('z').shift())
        .enabled(s.doc.redo_label().is_some()),
    );
    menus.push(edit);

    let mut view = Menu::new("View");
    view.push(
        cmd(Command::Browser, "Browser")
            .shortcut(Shortcut::cmd('b').alt())
            .checked(p.show_browser),
    );
    view.push(
        cmd(Command::Inspector, "Inspector")
            .shortcut(Shortcut::cmd('i').alt())
            .checked(p.show_inspector),
    );
    view.push(
        cmd(Command::Scripts, "Scripts")
            .shortcut(key(ShortcutKey::F(8)))
            .checked(p.show_scripts),
    );
    view.push(
        cmd(Command::Editor, "Editor")
            .shortcut(Shortcut::cmd('e').alt())
            .checked(p.show_editor),
    );
    view.separator();
    for (tab, _, label, _) in crate::layout::DOCK_TABS {
        let f = match tab {
            DockTab::Rack => 6,
            DockTab::PianoRoll => 7,
            DockTab::Mixer => 9,
            DockTab::Perform => 10,
        };
        view.push(
            cmd(Command::Dock(tab), label)
                .shortcut(key(ShortcutKey::F(f)))
                .checked(p.show_editor && studio.ws.dock == tab),
        );
    }
    view.separator();
    view.push(cmd(Command::TempoLane, "Tempo lane").checked(studio.ws.playlist.show_tempo));
    view.push(cmd(Command::MasterMeter, "Master meter").checked(p.show_meter));
    view.push(cmd(Command::StatusDetails, "Device in status bar").checked(p.show_status_details));
    view.separator();
    view.push(cmd(Command::ResetLayout, "Reset layout"));
    menus.push(view);

    let song = s.mode() == PlayMode::Song;
    let mut transport = Menu::new("Transport");
    transport.push(
        cmd(
            Command::PlayStop,
            if s.is_playing() { "Stop" } else { "Play" },
        )
        .shortcut(key(ShortcutKey::Space)),
    );
    transport.push(cmd(Command::Record, "Record").checked(studio.recorder.armed));
    transport.separator();
    transport.push(cmd(Command::ToStart, "Go to start").shortcut(key(ShortcutKey::Home)));
    transport.push(cmd(Command::ToEnd, "Go to end").shortcut(key(ShortcutKey::End)));
    transport.separator();
    transport.push(
        cmd(Command::SongMode, "Song mode")
            .shortcut(key(ShortcutKey::Char('l')))
            .checked(song),
    );
    transport.push(
        cmd(Command::LoopSong, "Loop song")
            .checked(song && s.looping())
            .enabled(song),
    );
    menus.push(transport);

    if mac {
        let mut window = Menu::new("Window").role(MenuRole::Window);
        window.push(MenuEntry::Standard(StandardItem::Minimize));
        window.push(MenuEntry::Standard(StandardItem::Zoom));
        window.push(MenuEntry::Standard(StandardItem::Fullscreen));
        window.separator();
        window.push(MenuEntry::Standard(StandardItem::BringAllToFront));
        menus.push(window);
    }

    let mut help = Menu::new("Help").role(MenuRole::Help);
    help.push(cmd(Command::StartScreen, "Start screen"));
    help.push(
        cmd(Command::Keys, "Keyboard shortcuts")
            .shortcut(key(ShortcutKey::F(1)))
            .checked(studio.show_keys),
    );
    help.push(cmd(Command::Hints, "Usage hints").checked(p.show_help));
    if !mac && cfg!(target_os = "windows") {
        help.separator();
        help.push(MenuEntry::Standard(StandardItem::About {
            name: "Peachey Studio".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        }));
    }
    menus.push(help);

    MenuBar { menus }
}

/// Runs menu commands queued this frame and the shortcuts of every menu
/// command, in one place.
pub fn frame(ui: &mut Ui, studio: &mut Studio) {
    // While the save prompt is up it is the only thing that acts.
    if studio.pending.is_some() {
        ui.take_menu_commands();
        studio.prompt_keys(ui);
        return;
    }
    // So is the Save as template dialog; it takes the keys when it draws.
    if studio.template_dialog.is_some() {
        ui.take_menu_commands();
        studio.hold_dialog_keys(ui);
        return;
    }
    for id in ui.take_menu_commands() {
        let Some(c) = Command::from_id(id) else {
            continue;
        };
        // A native menu can take Cmd+Z from a text field; editing text
        // does not undo the project.
        if ui.text_focus() && matches!(c, Command::Undo | Command::Redo) {
            continue;
        }
        if studio.welcome && !c.on_start_screen() {
            continue;
        }
        run(ui, studio, c);
        if studio.pending.is_some() {
            studio.prompt_keys(ui);
            return;
        }
        if studio.template_dialog.is_some() {
            studio.hold_dialog_keys(ui);
            return;
        }
    }
    if ui.text_focus() {
        return;
    }
    let bar = bar(studio, false);
    let mut fired = Vec::new();
    for c in bar.commands() {
        if let Some(s) = &c.shortcut
            && let Some(command) = Command::from_id(c.id)
            && (!studio.welcome || command.on_start_screen())
            && ui.input.take_shortcut(s)
        {
            fired.push(command);
        }
    }
    // Ctrl+Y redoes too.
    if !studio.welcome && ui.input.take_command("y", false) {
        fired.push(Command::Redo);
    }
    for c in fired {
        if studio.pending.is_some() || studio.template_dialog.is_some() {
            break;
        }
        run(ui, studio, c);
    }
    if studio.pending.is_some() {
        studio.prompt_keys(ui);
    }
    if studio.template_dialog.is_some() {
        studio.hold_dialog_keys(ui);
    }
}

pub fn run(ui: &mut Ui, studio: &mut Studio, c: Command) {
    match c {
        Command::NewProject => studio.request(Change::New),
        Command::Template(i) => studio.request(Change::Template(i)),
        Command::Demo(i) => studio.request(Change::Demo(i)),
        Command::UserTemplate(slot) => match studio.templates.get(slot) {
            Some(entry) => {
                let path = entry.path.clone();
                studio.request(Change::UserTemplate(path));
            }
            None => studio
                .session
                .notify("That template is no longer there", true),
        },
        Command::SaveTemplate => studio.open_save_template(),
        Command::StartScreen => {
            studio.templates.refresh();
            studio.welcome = true;
        }
        Command::Open => studio.open(),
        Command::Recent(i) => {
            if let Some(path) = studio.prefs.existing_recent().get(i).cloned() {
                studio.request(Change::OpenPath(path));
            }
        }
        Command::ClearRecent => {
            studio.prefs.recent.clear();
            studio.prefs.save();
        }
        Command::Close => studio.request(Change::Close),
        Command::Save => {
            studio.save();
        }
        Command::SaveAs => {
            studio.save_as();
        }
        Command::ExportAudio => studio.export(),
        Command::ExportMidi => studio.export_midi(),
        Command::Settings => {
            let at = ui.screen();
            let anchor = Rect {
                x: at.right() - 40.0,
                y: at.bottom() - crate::studio::STATUS_H,
                w: 24.0,
                h: crate::studio::STATUS_H,
            };
            studio.welcome = false;
            ui.open_popup(W::new("settings-panel"), anchor);
        }
        Command::Quit => studio.request(Change::Quit),
        Command::Undo => {
            if let Some(label) = studio.session.doc.undo() {
                studio.session.notify(format!("Undo {label}"), false);
            }
        }
        Command::Redo => {
            if let Some(label) = studio.session.doc.redo() {
                studio.session.notify(format!("Redo {label}"), false);
            }
        }
        Command::Browser => crate::layout::toggle_browser(studio),
        Command::Inspector => crate::layout::toggle_inspector(studio),
        Command::Editor => crate::layout::toggle_editor(studio),
        Command::Scripts => crate::scripts::toggle(studio),
        Command::Dock(tab) => crate::layout::dock_tab(studio, tab, false),
        Command::TempoLane => studio.ws.playlist.show_tempo = !studio.ws.playlist.show_tempo,
        Command::MasterMeter => {
            studio.prefs.show_meter = !studio.prefs.show_meter;
            studio.prefs.save();
        }
        Command::StatusDetails => {
            studio.prefs.show_status_details = !studio.prefs.show_status_details;
            studio.prefs.save();
        }
        Command::ResetLayout => crate::layout::reset_layout(studio),
        Command::PlayStop => studio.session.toggle_play(),
        Command::Record => studio.recorder.armed = !studio.recorder.armed,
        Command::ToStart => studio.session.seek(0.0),
        Command::ToEnd => {
            let to = studio.session.play_end();
            studio.session.seek(to);
        }
        Command::SongMode => {
            let next = match studio.session.mode() {
                PlayMode::Song => PlayMode::Pattern(studio.current_pattern()),
                PlayMode::Pattern(_) => PlayMode::Song,
            };
            studio.session.set_mode(next);
        }
        Command::LoopSong => {
            if studio.session.mode() == PlayMode::Song {
                let on = studio.session.looping();
                studio.session.set_looping(!on);
            }
        }
        Command::Keys => studio.show_keys = !studio.show_keys,
        Command::Hints => crate::layout::toggle_help(studio),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_ids_round_trip() {
        let mut all: Vec<Command> = SIMPLE.to_vec();
        all.extend(DOCKS.map(Command::Dock));
        all.extend([
            Command::Hints,
            Command::Template(0),
            Command::Template(9),
            Command::Recent(0),
            Command::Recent(9),
            Command::UserTemplate(0),
            Command::UserTemplate(crate::user_templates::MAX_TEMPLATES - 1),
        ]);
        let mut ids = std::collections::HashSet::new();
        for c in all {
            assert!(ids.insert(c.id()), "{c:?} shares an id");
            assert_eq!(Command::from_id(c.id()), Some(c));
        }
        assert_eq!(Command::from_id(0), None);
        // Existing ids never move; user templates stay inside their range.
        assert_eq!(Command::NewProject.id(), 1);
        assert_eq!(Command::Keys.id(), 29);
        assert_eq!(Command::Recent(999).id(), 2999);
        assert_eq!(Command::UserTemplate(0).id(), 3000);
        let past = Command::UserTemplate(crate::user_templates::MAX_TEMPLATES).id();
        assert_eq!(Command::from_id(past), None);
    }
}
