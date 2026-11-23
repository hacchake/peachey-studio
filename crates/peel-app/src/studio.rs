//! The main window: layout, global shortcuts and the computer keyboard.

use std::path::PathBuf;

use peel_document::Id;
use peel_session::{AudioExportSettings, ExportSettings, PlayMode, Session};
use peel_ui::Id as W;
use peel_ui::{Align, App, KeyCode, NamedKey, Rect, Ui, WindowControl};

use crate::prefs::Prefs;
use crate::welcome::{WelcomeAction, WelcomeData};
use crate::workspace::{DockTab, Focus, Workspace};
use crate::{browser, inspector, mixer, piano_roll, playlist, rack, transport};

pub struct Studio {
    pub session: Session,
    pub ws: Workspace,
    pub prefs: Prefs,
    pub welcome: bool,
    pub show_keys: bool,
    pub recorder: crate::recorder::Recorder,
    pub scripts: crate::scripts::Scripts,
    mcp: Option<peel_mcp::Bridge>,
    /// イベントループを起こす手段。MCP の要求が届いたら鳴らす。
    waker: Option<peel_ui::Waker>,
    pub(crate) recovered: Vec<peel_project::AutosaveEntry>,
    /// A project change waiting on the save prompt.
    pub pending: Option<crate::project::Change>,
    /// The user's saved templates.
    pub templates: crate::user_templates::Library,
    /// The Save as template dialog, while open.
    pub template_dialog: Option<crate::user_templates::SaveDialog>,
    /// Quit was confirmed; the window closes after this frame.
    pub quit: bool,
    /// Show the in-window menu strip even beside a platform menu bar
    /// (`PEEL_FILE_MENU`, for screenshots).
    pub menu_strip: bool,
    /// `PEEL_PLUGIN` debug insert still waiting for the plugin scan.
    debug_plugin: Option<String>,
    last_autosave: std::time::Instant,
    autosaved_revision: u64,
    /// The dock tab last frame; switching editors reveals a hidden dock.
    last_dock: DockTab,
    /// The channel the computer keyboard played last frame; changing it
    /// releases held keys.
    last_channel: Option<Id>,
    /// `PEEL_MPE_TAKE` screenshot performance.
    pub dev_take: crate::dev_take::DevTake,
}

/// Seconds between autosaves of a changed project.
const AUTOSAVE_EVERY: f32 = 60.0;

/// Seam between panes: thin enough to read as a separator, the splitter
/// grab zone reaches a little past it.
pub const GAP: f32 = 2.0;
/// Top bar and status bar heights.
pub const TOP_H: f32 = 40.0;
pub const STATUS_H: f32 = 24.0;

impl Studio {
    pub fn new(session: Session) -> Studio {
        let mut ws = Workspace::default();
        ws.selected_channel = session
            .project()
            .channels
            .iter()
            .find(|c| c.name == "Piano")
            .or(session.project().channels.first())
            .map(|c| c.id);
        ws.focus = ws.selected_channel.map(Focus::Channel);
        let prefs = Prefs::load();
        prefs.apply_layout(&mut ws);
        ws.octave = prefs.octave;
        if let Some(dir) = prefs.sample_dir.clone().filter(|d| d.is_dir()) {
            ws.sample_dir = Some(dir);
        }
        let recovered = if cfg!(test) {
            Vec::new()
        } else {
            peel_project::list_autosaves(&crate::prefs::autosave_dir()).unwrap_or_default()
        };
        let screenshot = std::env::var_os("PEEL_SCREENSHOT").is_some();
        let welcome = !cfg!(test) && !screenshot && (prefs.show_welcome || !recovered.is_empty());
        let revision = session.doc.revision();
        let last_dock = ws.dock;
        let mut studio = Studio {
            session,
            ws,
            prefs,
            welcome: welcome || std::env::var_os("PEEL_WELCOME").is_some(),
            show_keys: std::env::var_os("PEEL_KEYS").is_some(),
            recorder: Default::default(),
            scripts: Default::default(),
            mcp: None,
            waker: None,
            recovered,
            pending: None,
            templates: crate::user_templates::Library::open(crate::user_templates::library_dir()),
            template_dialog: None,
            quit: false,
            menu_strip: std::env::var_os("PEEL_FILE_MENU").is_some(),
            debug_plugin: std::env::var("PEEL_PLUGIN").ok(),
            last_autosave: std::time::Instant::now(),
            autosaved_revision: revision,
            last_dock,
            last_channel: None,
            dev_take: Default::default(),
        };
        studio.scripts.python = studio.prefs.python.clone();
        studio.open_view();
        studio.apply_io_prefs();
        studio.apply_debug_env();
        studio
    }

    pub fn enable_mcp(&mut self) {
        match peel_mcp::Bridge::start_stdio() {
            Ok(bridge) => {
                self.mcp = Some(bridge);
                self.welcome = false;
                self.connect_waker();
            }
            Err(error) => self
                .session
                .notify(format!("Could not start MCP: {error}"), true),
        }
    }

    /// MCP の要求が届いたらイベントループを起こすようにする。待機中や
    /// 最小化中は描画フレームが来ないので、これがないと要求が溜まったまま
    /// 処理されない。ブリッジと起こし役のどちらが先に揃っても繋がる。
    fn connect_waker(&mut self) {
        if let (Some(bridge), Some(waker)) = (&self.mcp, &self.waker) {
            let waker = waker.clone();
            bridge.set_waker(move || waker.wake());
        }
    }

    /// 届いている MCP の要求を処理し、その結果に UI の状態を合わせる。
    fn pump_mcp(&mut self) {
        let Some(bridge) = &mut self.mcp else { return };
        let project = self.session.project().id;
        let recording = self.session.is_recording();
        let live_target = self.session.live_target();
        bridge.pump(&mut self.session);
        if recording != self.session.is_recording() {
            self.recorder.armed = self.session.is_recording();
        }
        if live_target != self.session.live_target() {
            self.ws.selected_channel = self.session.live_target();
        }
        if project != self.session.project().id {
            self.reset_workspace();
        }
    }

    /// View defaults for a newly opened project: an arranged song opens in
    /// Song mode, with the tempo strip shown when the tempo changes.
    fn open_view(&mut self) {
        let p = self.session.project();
        let arranged = !p.arrangement().clips.is_empty();
        self.ws.playlist.show_tempo = p.tempo.len() > 1;
        if arranged {
            self.session.set_mode(peel_session::PlayMode::Song);
        }
    }

    pub(crate) fn reset_workspace(&mut self) {
        let sample_dir = self.ws.sample_dir.clone();
        let octave = self.ws.octave;
        self.ws = Workspace::default();
        // Pane sizes and tabs are the user's, not the project's.
        self.prefs.apply_layout(&mut self.ws);
        self.last_dock = self.ws.dock;
        self.ws.sample_dir = sample_dir;
        self.ws.octave = octave;
        self.ws.selected_channel = self
            .session
            .project()
            .channels
            .iter()
            .find(|c| c.name == "Piano")
            .or(self.session.project().channels.first())
            .map(|c| c.id);
        self.ws.focus = self.ws.selected_channel.map(Focus::Channel);
        self.open_view();
        self.apply_io_prefs();
        self.autosaved_revision = self.session.doc.revision();
    }

    /// Applies the machine's saved MIDI channel filter, controller
    /// mappings and secondary outputs to the session. Mappings and routes
    /// that name things this project does not have are skipped and counted.
    pub(crate) fn apply_io_prefs(&mut self) {
        let prefs = self.prefs.clone();
        if self.session.midi_channel_filter() != prefs.midi_channel
            && let Err(e) = self.session.set_midi_channel_filter(prefs.midi_channel)
        {
            self.session.notify(e, true);
        }
        let range = if prefs.pitch_bend_range.is_finite() {
            prefs.pitch_bend_range.clamp(0.0, 48.0)
        } else {
            2.0
        };
        if (self.session.pitch_bend_range() - range).abs() > 1.0e-6
            && let Err(e) = self.session.set_pitch_bend_range(range)
        {
            self.session.notify(e, true);
        }
        // Saved MPE setup, applied once here; RPN messages from a controller
        // may change it afterwards and the editor shows what is live.
        let mpe = if prefs.mpe.validate().is_ok() {
            prefs.mpe
        } else {
            peel_session::MpeConfig::default()
        };
        // Off is always sent: a controller's RPN 6 can leave the live mode
        // Off while the session still listens for that zone.
        if (mpe.mode == peel_session::MpeMode::Off || self.session.mpe_config() != mpe)
            && let Err(e) = self.session.configure_mpe(mpe)
        {
            self.session.notify(e, true);
        }
        if !prefs.midi_mappings.is_empty() || !self.session.midi_control_mappings().is_empty() {
            let _ = self.session.set_midi_control_mappings(Vec::new());
            let skipped = prefs
                .midi_mappings
                .iter()
                .filter(|m| self.session.add_midi_control_mapping((*m).clone()).is_err())
                .count();
            if skipped > 0 {
                self.session.notify(
                    format!(
                        "{skipped} MIDI mapping{} belong to another project",
                        if skipped == 1 { "" } else { "s" }
                    ),
                    false,
                );
            }
        }
        let usable: Vec<_> = prefs
            .output_routes
            .iter()
            .filter(|r| {
                r.track
                    .is_none_or(|t| self.session.project().mixer.track(t).is_some())
            })
            .cloned()
            .collect();
        if usable != self.session.output_routes()
            && let Err(e) = self.session.configure_output_routes(usable)
        {
            self.session.notify(format!("Outputs: {e}"), true);
        }
        if let Some(dir) = prefs.recordings_dir.clone().filter(|d| d.is_dir()) {
            self.session.set_recordings_dir(Some(dir));
        }
        if let Some(c) = prefs.primary_output
            && self.session.primary_output_channels() != c
            && let Err(e) = self.session.set_primary_output_channels(c)
        {
            self.session.notify(format!("Main output: {e}"), true);
        }
    }

    /// Ends every computer-keyboard note still held (focus loss, a new
    /// target or a text field taking the keys would otherwise leave them
    /// sounding, because their key-up never arrives).
    pub(crate) fn release_held(&mut self) {
        let held: Vec<_> = self.ws.held.drain().collect();
        for (_, (ch, key)) in held {
            self.session.live_note(ch, key, 0.0, false);
        }
    }

    fn welcome(&mut self, ui: &mut Ui) {
        let recent = self.prefs.existing_recent();
        let mut show = self.prefs.show_welcome;
        // Under the save prompt the keys are the prompt's.
        let keys = if self.pending.is_some() {
            std::mem::take(&mut ui.input.keys)
        } else {
            Vec::new()
        };
        let action = crate::welcome::draw(
            ui,
            WelcomeData {
                recent: &recent,
                recovered: &self.recovered,
                templates: &self.templates,
                show_at_startup: &mut show,
            },
        );
        if self.pending.is_some() {
            ui.input.keys = keys;
        }
        if show != self.prefs.show_welcome {
            self.prefs.show_welcome = show;
            self.prefs.save();
        }
        let Some(action) = action else { return };
        if self.pending.is_some() {
            return;
        }
        use crate::project::Change;
        match action {
            WelcomeAction::New => self.request(Change::New),
            WelcomeAction::Template(i) => self.request(Change::Template(i)),
            WelcomeAction::Demo(i) => self.request(Change::Demo(i)),
            WelcomeAction::UserTemplate(p) => self.request(Change::UserTemplate(p)),
            WelcomeAction::Open => self.open(),
            WelcomeAction::OpenPath(p) => self.request(Change::OpenPath(p)),
            WelcomeAction::Recover(p) => self.request(Change::Recover(p)),
            WelcomeAction::Close => self.welcome = false,
        }
    }

    /// Periodically saves changed work to the autosave area off the UI
    /// thread so a crash never loses more than a minute.
    fn autosave(&mut self, force: bool) {
        // Tests, screenshots and other debug runs leave the user's data alone.
        if cfg!(test)
            || std::env::var_os("PEEL_NO_PREFS").is_some()
            || std::env::var_os("PEEL_SCREENSHOT").is_some()
        {
            return;
        }
        let rev = self.session.doc.revision();
        if rev == self.autosaved_revision || !self.session.doc.is_dirty() {
            return;
        }
        if !force && self.last_autosave.elapsed().as_secs_f32() < AUTOSAVE_EVERY {
            return;
        }
        self.last_autosave = std::time::Instant::now();
        self.autosaved_revision = rev;
        self.session.capture_plugin_states();
        let project = self.session.project().clone();
        let source = self
            .session
            .media_source()
            .map(std::path::Path::to_path_buf);
        let root = crate::prefs::autosave_dir();
        let job = move || {
            let r = match &source {
                Some(src) => peel_project::autosave_from(&project, &root, src),
                None => peel_project::autosave(&project, &root),
            };
            if let Err(e) = r {
                log::warn!("autosave failed: {e}");
            }
        };
        if force {
            job();
        } else {
            std::thread::spawn(job);
        }
    }

    fn saved(&mut self, path: &std::path::Path) {
        self.prefs.add_recent(path);
        let id = self.session.project().id;
        let _ = peel_project::discard_autosave(&crate::prefs::autosave_dir(), id);
        self.autosaved_revision = self.session.doc.revision();
    }

    /// `PEEL_DOCK=rack|roll|mixer`, `PEEL_FOCUS=master` and `PEEL_PLAY=1`
    /// set up a view for screenshots and manual checks.
    fn apply_debug_env(&mut self) {
        match std::env::var("PEEL_DOCK").as_deref() {
            Ok("roll") => self.ws.dock = DockTab::PianoRoll,
            Ok("mixer") => self.ws.dock = DockTab::Mixer,
            Ok("perform") => self.ws.dock = DockTab::Perform,
            _ => {}
        }
        if let Some(path) = std::env::var_os("PEEL_AUDIO") {
            let lane = self.session.project().arrangement().tracks[3].id;
            if let Err(e) = self.session.add_audio_clip(
                lane,
                peel_document::Time::ZERO,
                std::path::Path::new(&path),
            ) {
                log::error!("PEEL_AUDIO: {e}");
            }
            if let Some(c) = self
                .session
                .project()
                .arrangement()
                .clips
                .iter()
                .find(|c| c.track == lane)
            {
                self.ws.focus = Some(Focus::Clip(c.id));
                let id = c.id;
                if std::env::var_os("PEEL_FOLLOW").is_some() {
                    self.session.doc.edit("Follow", |p| {
                        if let Some(c) = p.arrangement_mut().clips.iter_mut().find(|c| c.id == id)
                            && let peel_document::ClipContent::Audio { source_bpm, .. } =
                                &mut c.content
                        {
                            *source_bpm = Some(100.0);
                        }
                    });
                }
            }
        }
        if let Some(path) = std::env::var_os("PEEL_SAMPLE")
            && let Some(id) =
                crate::rack::add_sample_channel(&mut self.session, std::path::Path::new(&path))
        {
            self.ws.selected_channel = Some(id);
            self.ws.focus = Some(Focus::Channel(id));
            if let Some(dev) = self.session.project().channel(id).map(|c| c.device.id) {
                self.session.set_param(dev, "sample.start", 0.18, 1);
                self.session.doc.end_gesture();
            }
        }
        if std::env::var_os("PEEL_PERFORM").is_some() {
            self.session
                .replace_project(peel_session::template::house());
            self.ws.dock = DockTab::Perform;
            self.session.launch_scene(0);
        }
        if std::env::var_os("PEEL_SIDECHAIN").is_some() {
            let p = self.session.project();
            let kick = p
                .channels
                .iter()
                .find(|c| c.name == "Kick")
                .map(|c| c.track);
            let synth = p
                .channels
                .iter()
                .find(|c| c.name == "Synth")
                .map(|c| c.track);
            if let (Some(kick), Some(synth)) = (kick, synth) {
                let mut comp =
                    peel_session::template::device("peel.fx.compressor", Some("Sidechain Duck"));
                comp.sidechain = Some(kick);
                comp.preset = Some("Sidechain Duck".into());
                self.session.doc.edit("Sidechain", |p| {
                    if let Some(t) = p.mixer.track_mut(synth) {
                        t.inserts.insert(0, comp);
                    }
                });
                self.ws.focus = Some(Focus::Track(synth));
                self.ws.dock = DockTab::Mixer;
            }
        }
        if std::env::var_os("PEEL_TEMPO").is_some() {
            self.ws.playlist.show_tempo = true;
            self.session.doc.edit("Tempo", |p| {
                use peel_document::Time;
                p.tempo = vec![
                    peel_time::TempoSegment {
                        start: Time::ZERO,
                        bpm: 118.0,
                        ramp_to: Some(140.0),
                    },
                    peel_time::TempoSegment {
                        start: Time::beats(16),
                        bpm: 140.0,
                        ramp_to: None,
                    },
                    peel_time::TempoSegment {
                        start: Time::beats(24),
                        bpm: 104.0,
                        ramp_to: None,
                    },
                ];
            });
        }
        if std::env::var_os("PEEL_ARM").is_some() {
            for k in [3, 4] {
                let lane = self.session.project().arrangement().tracks[k].id;
                self.session.arm_lane(lane, true);
            }
        }
        // `PEEL_HIDE=browser,inspector,editor,status` and `PEEL_HELP=1` set pane
        // choices for screenshots (not saved: screenshots skip prefs).
        if let Ok(list) = std::env::var("PEEL_HIDE") {
            for pane in list.split(',') {
                match pane.trim() {
                    "browser" => self.prefs.show_browser = false,
                    "inspector" => self.prefs.set_inspector(false),
                    "editor" => self.prefs.show_editor = false,
                    "status" => self.prefs.show_status_details = false,
                    _ => {}
                }
            }
        }
        if std::env::var_os("PEEL_HELP").is_some() {
            self.prefs.show_help = true;
        }
        if let Ok(language) = std::env::var("PEEL_SCRIPTS") {
            self.scripts.open_requested = true;
            // A language name selects that action for screenshots.
            if language != "1" {
                self.scripts.ensure_scanned();
                self.scripts.selected = self
                    .scripts
                    .factory
                    .iter()
                    .position(|s| s.language.to_string().eq_ignore_ascii_case(&language))
                    .map(crate::scripts::Pick::Factory);
            }
        }
        if std::env::var_os("PEEL_GRAPH").is_some() {
            self.ws.rack_graph = true;
        }
        if let Ok(name) = std::env::var("PEEL_CHANNEL")
            && let Some(c) = self
                .session
                .project()
                .channels
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.id)
        {
            self.ws.selected_channel = Some(c);
            self.ws.focus = Some(Focus::Channel(c));
        }
        if std::env::var_os("PEEL_AUTOMATION").is_some() {
            use peel_document::{AutomationPoint, AutomationTarget, ClipContent, Time};
            let master = self.session.project().mixer.tracks[0].id;
            let synth = self
                .session
                .project()
                .mixer
                .tracks
                .iter()
                .find(|t| t.name == "Synth")
                .map(|t| t.id)
                .unwrap_or(master);
            let fade = crate::playlist::create_automation(
                &mut self.session,
                &mut self.ws,
                AutomationTarget::TrackGain { track: master },
                0.8,
            );
            let pan = crate::playlist::create_automation(
                &mut self.session,
                &mut self.ws,
                AutomationTarget::TrackPan { track: synth },
                0.5,
            );
            let pt = |b: i64, v: f32| AutomationPoint {
                position: Time::beats(b),
                value: v,
                tension: 0.0,
            };
            self.session.doc.edit("Shape automation", |p| {
                for c in p.arrangement_mut().clips.iter_mut() {
                    if let ClipContent::Automation { points, .. } = &mut c.content {
                        if c.id == fade {
                            *points = vec![pt(0, 0.8), pt(20, 0.8), pt(30, 0.15), pt(32, 0.0)];
                            points[1].tension = -0.6;
                        } else if c.id == pan {
                            *points = (0..=8)
                                .map(|i| pt(i * 4, if i % 2 == 0 { 0.15 } else { 0.85 }))
                                .collect();
                            for (i, x) in points.iter_mut().enumerate() {
                                x.tension = if i % 2 == 0 { 0.5 } else { -0.5 };
                            }
                        }
                    }
                }
            });
        }
        if std::env::var_os("PEEL_EQ").is_some() {
            let master = self.session.project().mixer.tracks[0].id;
            let eq = peel_session::template::device("peel.fx.eq", Some("Vocal Presence"));
            self.session
                .doc
                .edit("Add EQ", |p| p.mixer.tracks[0].inserts.insert(0, eq));
            self.ws.focus = Some(Focus::Track(master));
        }
        // `PEEL_FX=eq|mastering` opens that effect's editor on the master
        // (adding the effect first); `PEEL_FX_MODE=advanced` picks the
        // Mastering view, `PEEL_FX_TAB=resonance` the EQ's suggestions
        // (with `PEEL_FX_LISTEN=1` already listening) and
        // `PEEL_MIXER_SIZE=compact|full` the strip size. Real devices only.
        if let Ok(size) = std::env::var("PEEL_MIXER_SIZE") {
            self.ws.mixer_size = match size.as_str() {
                "compact" => crate::workspace::MixerSize::Compact,
                "full" => crate::workspace::MixerSize::Full,
                _ => crate::workspace::MixerSize::Expanded,
            };
        }
        if let Ok(fx) = std::env::var("PEEL_FX") {
            let master = self.session.project().mixer.tracks[0].id;
            match fx.as_str() {
                "eq" => {
                    let existing = self.session.project().mixer.tracks[0]
                        .inserts
                        .iter()
                        .find(|d| d.type_id == "peel.fx.eq")
                        .map(|d| d.id);
                    let id = existing.unwrap_or_else(|| {
                        let eq =
                            peel_session::template::device("peel.fx.eq", Some("Vocal Presence"));
                        let id = eq.id;
                        self.session
                            .doc
                            .edit("Add EQ", |p| p.mixer.tracks[0].inserts.insert(0, eq));
                        id
                    });
                    self.ws.fx.open(master, id);
                    self.ws.focus = Some(Focus::Track(master));
                    if std::env::var("PEEL_FX_TAB").as_deref() == Ok("resonance") {
                        self.ws.fx.eq.tab = crate::eq_editor::Tab::Resonance;
                    }
                    if std::env::var_os("PEEL_FX_LISTEN").is_some() {
                        self.ws.fx.eq.learn = crate::eq_editor::Learn::Listening;
                    }
                }
                "mastering" => {
                    crate::mixer::open_mastering(&mut self.session, &mut self.ws);
                    if std::env::var("PEEL_FX_MODE").as_deref() == Ok("advanced") {
                        self.ws.fx.mastering.advanced = true;
                    }
                }
                _ => {}
            }
        }
        // `PEEL_INSTRUMENT=<channel name>` opens that instrument's editor.
        if let Ok(name) = std::env::var("PEEL_INSTRUMENT")
            && let Some(c) = self
                .session
                .project()
                .channels
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.id)
        {
            crate::instrument_editor::open(&mut self.session, &mut self.ws, c);
        }
        // `PEEL_DEVICE=<native type id>` puts that instrument on the
        // selected channel (`PEEL_CHANNEL`, else the first) and opens its
        // editor, or inserts that effect on the master and opens it.
        // Real devices only.
        if let Ok(type_id) = std::env::var("PEEL_DEVICE") {
            match peel_devices::descriptor(&type_id) {
                Some(desc) if desc.kind == peel_dsp::DeviceKind::Instrument => {
                    let channel = self
                        .ws
                        .selected_channel
                        .filter(|&c| self.session.project().channel(c).is_some())
                        .or_else(|| self.session.project().channels.first().map(|c| c.id));
                    if let Some(c) = channel {
                        let dev = peel_session::template::device(&type_id, None);
                        self.session.doc.edit(format!("Use {}", desc.name), |p| {
                            if let Some(ch) = p.channels.iter_mut().find(|ch| ch.id == c) {
                                ch.device = dev;
                                ch.name = desc.name.to_string();
                            }
                        });
                        self.ws.selected_channel = Some(c);
                        self.ws.focus = Some(Focus::Channel(c));
                        crate::instrument_editor::open(&mut self.session, &mut self.ws, c);
                    }
                }
                Some(desc) => {
                    let master = self.session.project().mixer.tracks[0].id;
                    let dev = peel_session::template::device(&type_id, None);
                    let id = dev.id;
                    self.session.doc.edit(format!("Add {}", desc.name), |p| {
                        p.mixer.tracks[0].inserts.insert(0, dev);
                    });
                    self.ws.fx.open(master, id);
                    self.ws.focus = Some(Focus::Track(master));
                }
                None => log::error!("PEEL_DEVICE: unknown device type {type_id}"),
            }
        }
        if std::env::var_os("PEEL_MIXER_EXPAND").is_some() {
            self.ws.dock = DockTab::Mixer;
            self.ws.mixer_restore_h = Some(self.ws.dock_h);
        }
        if std::env::var_os("PEEL_AUDIO_SHEET").is_some() {
            self.ws.io_open = true;
        }
        if std::env::var_os("PEEL_EXPORT").is_some() {
            self.ws.export_panel = true;
        }
        if std::env::var_os("PEEL_SCALE").is_some() {
            self.ws.roll.scale = Some(crate::harmony::Scale { root: 9, kind: 1 });
        }
        if std::env::var("PEEL_FOCUS").as_deref() == Ok("master") {
            self.ws.focus = Some(Focus::Track(self.session.project().mixer.tracks[0].id));
        }
        if let Ok(name) = std::env::var("PEEL_PATTERN")
            && let Some(p) = self
                .session
                .project()
                .patterns
                .iter()
                .find(|p| p.name == name)
                .map(|p| p.id)
        {
            self.session.select_pattern(p);
        }
        if std::env::var("PEEL_SONG").is_ok() {
            self.session.set_mode(PlayMode::Song);
        }
        // `PEEL_AT=<bar>` moves the playhead to the start of that bar.
        if let Some(bar) = std::env::var("PEEL_AT")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
        {
            let len = self.session.project().time_signature.bar_length().as_f64();
            self.session.seek(f64::from(bar.saturating_sub(1)) * len);
        }
        if std::env::var("PEEL_PLAY").is_ok() {
            self.session.play();
        }
        // `PEEL_UNSAVED=1` puts up the save prompt (Close with an edit).
        if std::env::var_os("PEEL_UNSAVED").is_some() {
            self.session
                .doc
                .edit("Rename project", |p| p.meta.title = "Night drive".into());
            self.request(crate::project::Change::Close);
        }
    }

    fn shortcuts(&mut self, ui: &mut Ui) {
        if ui.text_focus() {
            return;
        }
        // Menu shortcuts (File, Edit, View, Transport, Help) ran in
        // `menus::frame`; these keys have no menu entry.
        let bar = self.session.project().time_signature.bar_length().as_f64();
        if ui.input.take_named(NamedKey::PageUp) {
            self.session.nudge(-bar);
        }
        if ui.input.take_named(NamedKey::PageDown) {
            self.session.nudge(bar);
        }
        if self.ws.fx.is_open() && !ui.any_popup() && ui.input.take_named(NamedKey::Escape) {
            self.ws.fx.close();
        }
        if self.ws.inst.is_open() && !ui.any_popup() && ui.input.take_named(NamedKey::Escape) {
            crate::instrument_editor::close(&mut self.session, &mut self.ws);
        }
        if self.ws.io_open && !ui.any_popup() && ui.input.take_named(NamedKey::Escape) {
            self.ws.io_open = false;
        }
        if ui.input.take_named(NamedKey::Escape) {
            self.session.stop_preview();
        }
        self.typing_keyboard(ui);
    }

    /// Selected pattern (the pattern-mode target, or the first pattern).
    pub fn current_pattern(&self) -> Id {
        current_pattern(&self.session)
    }

    fn typing_keyboard(&mut self, ui: &mut Ui) {
        // Two rows, like a tracker: Z row is the lower octave, Q row the upper.
        const LOWER: [(KeyCode, i32); 17] = [
            (KeyCode::KeyZ, 0),
            (KeyCode::KeyS, 1),
            (KeyCode::KeyX, 2),
            (KeyCode::KeyD, 3),
            (KeyCode::KeyC, 4),
            (KeyCode::KeyV, 5),
            (KeyCode::KeyG, 6),
            (KeyCode::KeyB, 7),
            (KeyCode::KeyH, 8),
            (KeyCode::KeyN, 9),
            (KeyCode::KeyJ, 10),
            (KeyCode::KeyM, 11),
            (KeyCode::Comma, 12),
            (KeyCode::KeyL, 13),
            (KeyCode::Period, 14),
            (KeyCode::Semicolon, 15),
            (KeyCode::Slash, 16),
        ];
        const UPPER: [(KeyCode, i32); 17] = [
            (KeyCode::KeyQ, 12),
            (KeyCode::Digit2, 13),
            (KeyCode::KeyW, 14),
            (KeyCode::Digit3, 15),
            (KeyCode::KeyE, 16),
            (KeyCode::KeyR, 17),
            (KeyCode::Digit5, 18),
            (KeyCode::KeyT, 19),
            (KeyCode::Digit6, 20),
            (KeyCode::KeyY, 21),
            (KeyCode::Digit7, 22),
            (KeyCode::KeyU, 23),
            (KeyCode::KeyI, 24),
            (KeyCode::Digit9, 25),
            (KeyCode::KeyO, 26),
            (KeyCode::Digit0, 27),
            (KeyCode::KeyP, 28),
        ];
        if !self.ws.keys_enabled {
            self.release_held();
            return;
        }
        let Some(channel) = self.ws.selected_channel else {
            return;
        };
        let base = (self.ws.octave + 1) * 12;
        let lookup = |code: KeyCode| {
            LOWER
                .iter()
                .chain(UPPER.iter())
                .find(|(c, _)| *c == code)
                .map(|(_, o)| *o)
        };
        let mut ons = Vec::new();
        for k in ui.input.keys.iter_mut() {
            if k.consumed || k.modifiers.command() || k.modifiers.ctrl || k.modifiers.alt {
                continue;
            }
            let Some(code) = k.code else { continue };
            if code == KeyCode::Minus || code == KeyCode::Equal {
                k.consumed = true;
                if !k.repeat {
                    self.ws.octave =
                        (self.ws.octave + if code == KeyCode::Equal { 1 } else { -1 }).clamp(0, 8);
                }
                continue;
            }
            // KeyL doubles as the mode switch when nothing else uses it.
            if code == KeyCode::KeyL {
                continue;
            }
            if let Some(off) = lookup(code) {
                k.consumed = true;
                if !k.repeat && !self.ws.held.contains_key(&code) {
                    let key = (base + off).clamp(0, 127) as u8;
                    ons.push((code, key));
                }
            }
        }
        for (code, key) in ons {
            self.ws.held.insert(code, (channel, key));
            self.session.live_note(channel, key, 0.8, true);
        }
        let released: Vec<KeyCode> = ui
            .input
            .keys_released
            .iter()
            .filter_map(|k| k.code)
            .collect();
        for code in released {
            if let Some((ch, key)) = self.ws.held.remove(&code) {
                self.session.live_note(ch, key, 0.0, false);
            }
        }
    }

    /// Saves to the project's file, or asks for one. True once the project
    /// is on disk; false when the save failed or the dialog was cancelled.
    pub fn save(&mut self) -> bool {
        if self.session.path.is_some() {
            self.save_to(None)
        } else {
            self.save_as()
        }
    }

    pub fn save_as(&mut self) -> bool {
        let name = format!("{}.peelproj", self.session.project().meta.title);
        match crate::dialogs::save(&name, "Peel project", &["peelproj"]) {
            Some(path) => self.save_to(Some(path)),
            None => false,
        }
    }

    fn save_to(&mut self, path: Option<PathBuf>) -> bool {
        // A take still buffered belongs in the file.
        self.recorder.finish_capture(&mut self.session);
        match self.session.save(path) {
            Ok(p) => {
                self.saved(&p);
                true
            }
            Err(e) => {
                self.session.notify(format!("Save failed: {e}"), true);
                false
            }
        }
    }

    /// Asks for a project or MIDI file, then opens it (behind the save
    /// prompt when there is unsaved work).
    pub fn open(&mut self) {
        let Some(path) = crate::dialogs::open(&[
            ("Peel project", &["peelproj"]),
            ("MIDI file", &["mid", "midi"]),
        ]) else {
            return;
        };
        self.open_path(path);
    }

    pub(crate) fn prepare_project_change(&mut self) {
        self.recorder.finish_capture(&mut self.session);
        // Keep the outgoing take recoverable before replacing its document.
        self.autosave(true);
    }

    /// Opens a project or MIDI file (behind the save prompt when there is
    /// unsaved work).
    pub fn open_path(&mut self, path: PathBuf) {
        self.request(crate::project::Change::OpenPath(path));
    }

    /// Loads `path` now. False (and the current project kept) on failure.
    pub(crate) fn load_path(&mut self, path: PathBuf) -> bool {
        self.silence();
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        let ok = if ext == "mid" || ext == "midi" {
            crate::midi::import(&mut self.session, &path)
        } else {
            match self.session.open(&path) {
                Ok(()) => true,
                Err(e) => {
                    self.session
                        .notify(format!("Could not open project: {e}"), true);
                    false
                }
            }
        };
        if ok {
            self.welcome = false;
            self.reset_workspace();
            if ext == "peelproj" {
                self.prefs.add_recent(&path);
                self.prefs.save();
            }
        }
        ok
    }

    /// Opens the export panel (from the shortcut or the toolbar).
    pub fn export(&mut self) {
        self.ws.export_panel = true;
    }

    /// Renders the arrangement with the panel's audio settings.
    pub fn export_audio(&mut self) {
        if self.ws.export.is_some() {
            return;
        }
        let format = self.ws.export_format;
        let name = format!(
            "{}.{}",
            self.session.project().meta.title,
            format.extension()
        );
        let filter = format!("{} audio", format.label());
        let Some(path) = crate::dialogs::save(&name, &filter, &[format.extension()]) else {
            return;
        };
        let settings = AudioExportSettings {
            render: ExportSettings {
                path,
                sample_rate: self.ws.export_rate,
                bits: self.ws.export_bits,
                tail_seconds: self.ws.export_tail,
            },
            format,
            bitrate_kbps: self.ws.export_bitrate,
            dither: self.ws.export_dither,
            mono: self.ws.export_mono,
            normalize: self.ws.export_normalize,
        };
        self.ws.export = Some(if self.ws.export_stems {
            self.session.export_stems(settings)
        } else {
            self.session.export(settings)
        });
    }

    /// Writes the arrangement as a Standard MIDI File.
    pub fn export_midi(&mut self) {
        let name = format!("{}.mid", self.session.project().meta.title);
        let Some(path) = crate::dialogs::save(&name, "MIDI file", &["mid"]) else {
            return;
        };
        match peel_midi::export_arrangement(
            self.session.project(),
            &peel_midi::ExportOptions::default(),
        ) {
            Ok(out) => match std::fs::write(&path, &out.bytes) {
                Ok(()) => self.session.notify(
                    out.report.warnings.first().map_or_else(
                        || format!("Exported {}", path.display()),
                        |warning| format!("Exported MIDI: {warning}"),
                    ),
                    false,
                ),
                Err(e) => self
                    .session
                    .notify(format!("Could not write MIDI: {e}"), true),
            },
            Err(e) => self
                .session
                .notify(format!("Could not export MIDI: {e}"), true),
        }
    }

    fn status_bar(&mut self, ui: &mut Ui, r: Rect) {
        let t = ui.theme.clone();
        let mut row = r.shrink2(12.0, 0.0);
        if let Some(f) = self.session.render_progress() {
            let bar = row.cut_left(120.0).shrink2(0.0, 9.0);
            ui.scene.rect(bar, 3.0, t.control);
            ui.scene.rect(bar.with_w(bar.w * f), 3.0, t.accent);
            row.cut_left(8.0);
            ui.text(
                row.cut_left(80.0),
                "Rendering",
                t.caption,
                t.text_dim,
                Align::Start,
            );
        }
        if let Some(rec) = self.session.recording_status() {
            let mut cell = row.cut_left(118.0);
            let pulse = 0.6 + 0.4 * (ui.time() as f32 * 4.0).sin().abs();
            ui.scene.circle(
                peel_ui::vec2(cell.x + 5.0, cell.center().y),
                4.5,
                t.record.alpha(pulse),
            );
            cell.cut_left(16.0);
            let secs = rec.seconds as u64;
            let label = if rec.waiting_for_transport {
                "REC ready".to_string()
            } else {
                format!("REC {}:{:02}", secs / 60, secs % 60)
            };
            ui.text(cell, &label, t.caption, t.record, Align::Start);
            if let Some(e) = rec.error.as_deref() {
                let er = row.cut_left(320.0);
                let e = ui.fit_text(e, t.caption, er.w);
                ui.text(er, &e, t.caption, t.warn, Align::Start);
            }
            row.cut_left(8.0);
        }
        if let Some(job) = &self.ws.export {
            job.idle();
            if let Some(result) = job.take_result() {
                match result {
                    Ok(p) if p.is_dir() => self
                        .session
                        .notify(format!("Exported stems to {}", p.display()), false),
                    Ok(p) => self
                        .session
                        .notify(format!("Exported {}", p.display()), false),
                    Err(e) => self.session.notify(e, true),
                }
                self.ws.export = None;
            } else {
                let bar = row.cut_left(160.0).shrink2(0.0, 9.0);
                ui.scene.rect(bar, 3.0, t.control);
                ui.scene
                    .rect(bar.with_w(bar.w * job.progress()), 3.0, t.accent);
                row.cut_left(8.0);
                ui.text(
                    row.cut_left(80.0),
                    "Exporting",
                    t.caption,
                    t.text_dim,
                    Align::Start,
                );
                if ui
                    .button(W::new("cancel-export"), row.cut_left(64.0), "Cancel")
                    .clicked
                {
                    job.cancel();
                }
                ui.request_repaint();
            }
        }
        let audio_w = ui.quiet_button_width(true, "Audio", false);
        let audio = ui.quiet_button(
            W::new("audio-open"),
            row.cut_right(audio_w).shrink2(0.0, 1.0),
            Some(peel_ui::icons::HEADPHONES),
            "Audio",
            false,
            self.ws.io_open,
        );
        ui.tooltip(&audio, "Input, recording and output routing");
        if audio.clicked {
            self.ws.io_open = !self.ws.io_open;
            if self.ws.io_open {
                crate::instrument_editor::close(&mut self.session, &mut self.ws);
                self.ws.fx.close();
            }
        }
        row.cut_right(6.0);
        let gear = ui.icon_button(
            W::new("settings-open"),
            row.cut_right(24.0),
            peel_ui::icons::GEAR,
            ui.is_popup_open(W::new("settings-panel")),
            None,
        );
        ui.tooltip(&gear, "Audio and MIDI");
        if gear.clicked {
            ui.open_popup(W::new("settings-panel"), gear.rect);
        }
        // MIDI activity light.
        let activity = self.session.midi_activity();
        let dt = ui.dt() as f64;
        let lit = {
            let st = ui.state::<(u64, f64)>(W::new("midi-activity"));
            if st.0 != activity {
                *st = (activity, 0.0);
            }
            st.1 += dt;
            st.1 < 0.15 && activity > 0
        };
        let dot = row.cut_right(14.0);
        let t2 = ui.theme.clone();
        ui.scene.circle(
            dot.center(),
            3.0,
            if lit { t2.play } else { t2.control_hover },
        );
        if lit {
            ui.request_repaint();
        }
        row.cut_right(10.0);
        if let Some(n) = self.session.notice().cloned() {
            let c = if n.error { t.record } else { t.text_dim };
            ui.text(
                row.cut_left(row.w * 0.6),
                &n.text,
                t.caption,
                c,
                Align::Start,
            );
            ui.request_repaint();
        } else if !self.session.missing_devices().is_empty() {
            let msg = format!(
                "Missing devices: {}",
                self.session.missing_devices().join(", ")
            );
            ui.text(
                row.cut_left(row.w * 0.6),
                &msg,
                t.caption,
                t.warn,
                Align::Start,
            );
        }
        // Audio health: a lost or failing device gets a visible restart.
        if !self.session.audio_is_healthy() {
            let r = row.cut_right(118.0);
            let resp = ui.interact_button(W::new("audio-restart"), r, "Restart audio");
            ui.scene.rect(
                r.shrink2(0.0, 2.0),
                t.radius_s,
                t.record.alpha(if resp.hovered { 0.3 } else { 0.18 }),
            );
            let mut inner = r.shrink2(8.0, 0.0);
            ui.icon(
                inner.cut_left(14.0),
                peel_ui::icons::LIGHTNING,
                12.0,
                t.record,
            );
            inner.cut_left(4.0);
            ui.text(inner, "Restart audio", t.caption, t.record, Align::Start);
            let why = self
                .session
                .audio_warning()
                .unwrap_or("The audio device stopped")
                .to_string();
            ui.tooltip(&resp, &why);
            if resp.clicked {
                let device = self.prefs.audio_device.clone();
                if let Err(e) = self.session.restart_audio(device, self.prefs.buffer_frames) {
                    self.session
                        .notify(format!("Could not start audio: {e}"), true);
                }
            }
            row.cut_right(8.0);
        }
        if self.prefs.show_status_details {
            let right = format!(
                "{}  ·  {:.0} kHz  ·  octave {}",
                self.session.device_name(),
                self.session.sample_rate() / 1000.0,
                self.ws.octave
            );
            let right = ui.fit_text(&right, t.caption, row.w * 0.5);
            ui.text(row, &right, t.caption, t.text_faint, Align::End);
        }
    }
}

pub fn current_pattern(session: &Session) -> Id {
    session.selected_pattern()
}

impl App for Studio {
    fn update(&mut self, ui: &mut Ui, win: &WindowControl) {
        let frame_start = std::time::Instant::now();
        self.update_inner(ui, win);
        if std::env::var_os("PEEL_FRAME_STATS").is_some() {
            let us = frame_start.elapsed().as_micros() as u64;
            let shapes = ui.scene.shape_count();
            let st = ui.state::<(u64, u64, u64)>(W::new("frame-stats"));
            st.0 += 1;
            st.1 += us;
            st.2 = st.2.max(us);
            if st.0 == 120 {
                log::info!(
                    "UI build: avg {} us, max {} us, {} shapes",
                    st.1 / 120,
                    st.2,
                    shapes
                );
                *st = (0, 0, 0);
            }
        }
    }

    fn close_requested(&mut self) -> bool {
        // Closing the window is Quit: unsaved work gets the save prompt.
        self.template_dialog = None;
        self.request(crate::project::Change::Quit);
        std::mem::take(&mut self.quit)
    }

    fn menu_bar(&mut self) -> Option<peel_ui::MenuBar> {
        Some(crate::menus::bar(self, cfg!(target_os = "macos")))
    }

    fn set_waker(&mut self, waker: peel_ui::Waker) {
        self.waker = Some(waker);
        self.connect_waker();
    }

    fn wake(&mut self) {
        // 最小化中でも要求に答える。続くフレームが来れば同じ処理がもう一度
        // 走るが、空のキューを覗くだけで済む。
        self.pump_mcp();
        self.session.tick();
    }

    fn shutdown(&mut self) {
        self.recorder.finish_capture(&mut self.session);
        self.session.panic();
        // Unsaved work stays recoverable; a clean session leaves nothing.
        if self.session.doc.is_dirty() {
            self.autosave(true);
        } else if !cfg!(test) {
            let _ = peel_project::discard_autosave(
                &crate::prefs::autosave_dir(),
                self.session.project().id,
            );
        }
        self.prefs.octave = self.ws.octave;
        self.prefs.sample_dir = self.ws.sample_dir.clone();
        self.prefs.save();
    }
}

impl Studio {
    fn update_inner(&mut self, ui: &mut Ui, win: &WindowControl) {
        // Retry any bend or pressure return the live queue refused, even
        // with the editor closed.
        crate::expression::flush(&mut self.session, &mut self.ws.inst.expr);
        // Screenshot hooks use accepted live events after MIDI startup settles.
        if !cfg!(test)
            && self.ws.inst.is_open()
            && std::env::var_os("PEEL_SCREENSHOT").is_some()
            && (std::env::var_os("PEEL_BEND").is_some()
                || std::env::var_os("PEEL_PRESSURE").is_some())
        {
            let n = ui.state::<u32>(W::new("shot-expression"));
            *n += 1;
            if *n >= 30 && self.session.live_target().is_some() {
                let get = |k: &str| {
                    std::env::var(k)
                        .ok()
                        .and_then(|v| v.parse::<f32>().ok())
                        .filter(|v| v.is_finite())
                };
                if let Some(v) = get("PEEL_BEND").map(|v| v.clamp(-1.0, 1.0))
                    && (self.session.pitch_bend_value() - v).abs() > 1e-5
                {
                    self.session.pitch_bend(v);
                }
                if let Some(v) = get("PEEL_PRESSURE").map(|v| v.clamp(0.0, 1.0))
                    && (self.session.channel_pressure_value() - v).abs() > 1e-5
                {
                    self.session.channel_pressure(v);
                }
            }
        }
        self.pump_mcp();
        self.session.tick();
        // Input meter: peak hold with a fall of about 20 dB a second.
        let (l, r) = self.session.input_level();
        let fall = 0.1f32.powf(ui.dt().clamp(0.0, 0.1));
        let m = &mut self.ws.input_meter;
        *m = ((m.0 * fall).max(l), (m.1 * fall).max(r));
        if let Some(type_id) = &self.debug_plugin
            && let Some(name) = self
                .session
                .plugins()
                .iter()
                .find(|p| peel_session::plugin_type_id(p) == *type_id)
                .map(|p| p.name.clone())
        {
            let master = self.session.project().mixer.tracks[0].id;
            crate::mixer::add_plugin_insert(&mut self.session, master, type_id, &name);
            self.ws.focus = Some(Focus::Track(master));
            self.ws.browser_tab = crate::workspace::BrowserTab::Effects;
            self.debug_plugin = None;
        }
        if self.last_channel != self.ws.selected_channel
            || ui.input.focus_lost
            || (ui.text_focus() && !self.ws.held.is_empty())
        {
            self.release_held();
            self.last_channel = self.ws.selected_channel;
        }
        if ui.input.focus_lost {
            crate::instrument_editor::release_all(&mut self.session, &mut self.ws);
        }
        // Two editors never stack: the one opened last wins (opening any
        // from the UI closes the others; scripts open the effect editor
        // without knowing about the rest).
        if self.ws.fx.is_open() && self.ws.inst.is_open() {
            crate::instrument_editor::close(&mut self.session, &mut self.ws);
        }
        if self.ws.fx.is_open() && self.ws.io_open {
            self.ws.io_open = false;
        }
        self.session.set_live_target(self.ws.selected_channel);
        crate::dev_take::frame(self, ui);
        self.session.poll_midi();
        self.recorder.frame(&mut self.session);
        crate::scripts::frame(ui, self);
        crate::menus::frame(ui, self);
        if !self.welcome && self.pending.is_none() && self.template_dialog.is_none() {
            self.shortcuts(ui);
        }
        self.autosave(false);
        let t = ui.theme.clone();
        let screen = ui.screen();
        ui.scene.rect(screen, 0.0, t.bg);

        crate::util::set_help(ui, self.prefs.show_help);
        crate::scripts::sync(self);
        // Switching editors (a menu, a script, a shortcut) reveals the dock.
        if self.ws.dock != self.last_dock {
            self.last_dock = self.ws.dock;
            if !self.prefs.show_editor {
                self.prefs.show_editor = true;
                self.prefs.save();
            }
        }

        // Top bar, status bar, then a navigation rail on each side. Panes
        // show between the rails as the user chooses (View menu, rails,
        // shortcuts, scripts); the playlist takes what is left.
        let mut area = screen;
        let top = area.cut_top(TOP_H);
        let status = area.cut_bottom(STATUS_H);
        transport::draw(ui, top, win, self);
        let left_rail = area.cut_left(t.rail_w);
        let right_rail = area.cut_right(t.rail_w);
        crate::layout::left_rail(ui, left_rail, self);
        crate::layout::right_rail(ui, right_rail, self);
        area.cut_bottom(GAP);

        // Side panes give way on small windows without losing the size the
        // user chose for larger ones. The seams are laid out here and their
        // splitters run after the panes draw, so a seam's grab zone wins
        // over pane edges (scrollbars, tabs) that reach into it.
        let browser_r = self.prefs.show_browser.then(|| {
            let w = self.ws.browser_w.min((area.w * 0.24).max(200.0));
            let r = area.cut_left(w);
            (r, area.cut_left(GAP), w)
        });
        let right_r = self.prefs.show_right().then(|| {
            let w = self.ws.inspector_w.min((area.w * 0.3).max(240.0));
            let r = area.cut_right(w);
            (r, area.cut_right(GAP), w)
        });
        let mut dock_seam = None;
        // An expanded mixer takes the whole area between the panes: the
        // playlist steps aside so strips can use the window's height.
        let mixer_expanded = self.prefs.show_editor
            && self.ws.dock == DockTab::Mixer
            && self.ws.mixer_restore_h.is_some();
        let dock_r = if mixer_expanded {
            let r = area;
            area = Rect { h: 0.0, ..area };
            r
        } else if self.prefs.show_editor {
            let max_dock = (area.h - 160.0).max(180.0);
            self.ws.dock_h = self.ws.dock_h.clamp(180.0, max_dock);
            let r = area.cut_bottom(self.ws.dock_h);
            dock_seam = Some((area.cut_bottom(GAP), max_dock));
            r
        } else {
            // Collapsed: only the header, so every editor stays one click away.
            let r = area.cut_bottom(t.header_h);
            area.cut_bottom(GAP);
            r
        };
        let playlist_r = area;

        if let Some((r, _, _)) = browser_r
            && browser::draw(ui, r, &mut self.session, &mut self.ws)
        {
            crate::layout::toggle_browser(self);
        }
        if !mixer_expanded {
            playlist::draw(ui, playlist_r, &mut self.session, &mut self.ws);
        }
        dock(ui, dock_r, self);
        if let Some((r, _, _)) = right_r {
            if self.prefs.show_scripts {
                crate::scripts::column(ui, r, self);
            } else {
                if inspector::draw(ui, r, &mut self.session, &mut self.ws) {
                    crate::layout::toggle_inspector(self);
                }
            }
        }
        if let Some((_, seam, w)) = browser_r {
            let d = ui.splitter(W::new("split-browser"), seam, true);
            if d != 0.0 {
                self.ws.browser_w = (w + d).clamp(180.0, 420.0);
            }
        }
        if let Some((_, seam, w)) = right_r {
            let d = ui.splitter(W::new("split-inspector"), seam, true);
            if d != 0.0 {
                self.ws.inspector_w = (w - d).clamp(240.0, 480.0);
            }
        }
        if let Some((seam, max_dock)) = dock_seam {
            let d = ui.splitter(W::new("split-dock"), seam, false);
            self.ws.dock_h = (self.ws.dock_h - d).clamp(180.0, max_dock);
        }
        self.status_bar(ui, status);
        crate::fx_editor::draw(
            ui,
            Rect {
                y: top.bottom(),
                h: status.y - top.bottom(),
                ..screen
            },
            &mut self.session,
            &mut self.ws,
        );
        let sheet_area = Rect {
            y: top.bottom(),
            h: status.y - top.bottom(),
            ..screen
        };
        crate::instrument_editor::draw(
            ui,
            sheet_area,
            &mut self.session,
            &mut self.ws,
            &mut self.prefs,
        );
        crate::audio_io::draw(
            ui,
            sheet_area,
            &mut self.session,
            &mut self.ws,
            &mut self.prefs,
            &mut self.recorder,
        );
        crate::layout::persist(ui, self);

        // A file dropped on no pane opens (one at a time: a second waits
        // for no prompt).
        let leftovers = std::mem::take(&mut ui.input.dropped_files);
        if let Some(path) = leftovers.into_iter().next() {
            self.open_path(path);
        }

        // `PEEL_FILE_MENU=1`: the in-window menus with File open.
        if std::env::var_os("PEEL_FILE_MENU").is_some()
            && ui.time() < 0.5
            && !ui.any_popup()
            && let Some(r) = ui.widget_rect(crate::menus::STRIP.with(("title", 0usize)))
        {
            ui.open_popup(crate::menus::STRIP.with(("menu", 0usize)), r);
        }
        if std::env::var_os("PEEL_SETTINGS").is_some()
            && !ui.is_popup_open(W::new("settings-panel"))
            && ui.time() < 0.5
        {
            ui.open_popup(W::new("settings-panel"), Rect::ZERO);
        }
        crate::settings::panel(ui, &mut self.session, &mut self.prefs, self.ws.input_meter);
        if self.welcome {
            self.welcome(ui);
        } else if self.show_keys && crate::keys::overlay(ui) {
            self.show_keys = false;
        }
        self.save_template_dialog(ui);
        self.save_prompt(ui);
        if self.quit {
            win.exit();
        }
        if self.session.is_playing() {
            ui.request_repaint();
        }
        let dirty = if self.session.doc.is_dirty() {
            " •"
        } else {
            ""
        };
        win.set_title(&format!(
            "{}{} — Peachey Studio",
            self.session.project().meta.title,
            dirty
        ));
    }
}

fn dock(ui: &mut Ui, r: Rect, studio: &mut Studio) {
    let t = ui.theme.clone();
    ui.scene.rect(r, 0.0, t.panel);
    let mut body = r;
    let head_full = body.cut_top(t.header_h);
    let mut head = head_full.shrink2(6.0, 0.0);
    let shown = studio.prefs.show_editor;
    if shown {
        ui.scene
            .hline(r.x, r.right(), head_full.bottom() - 0.5, t.line);
    }

    // Collapse / expand at the far right.
    let toggle_r = head.cut_right(28.0).shrink2(0.0, 4.0);
    let toggle = ui.icon_button(
        W::new("dock-toggle"),
        toggle_r,
        if shown {
            peel_ui::icons::CARET_DOWN
        } else {
            peel_ui::icons::CARET_UP
        },
        false,
        None,
    );
    ui.name(&toggle, if shown { "Hide editor" } else { "Show editor" });
    ui.tooltip(
        &toggle,
        if shown {
            "Hide the editor"
        } else {
            "Show the editor"
        },
    );
    if toggle.clicked {
        crate::layout::toggle_editor(studio);
    }
    head.cut_right(4.0);

    // Tabs keep their labels while the editor's tools still fit beside
    // them; otherwise every tab is an icon, so widths never jump.
    let labels_w: f32 = crate::layout::DOCK_TABS
        .iter()
        .map(|(_, _, label, _)| ui.tab_width(label))
        .sum();
    let has_picker = matches!(studio.ws.dock, DockTab::Rack | DockTab::PianoRoll);
    let picker_w = if has_picker { 196.0 } else { 0.0 };
    // What each editor's header tools take (see the editors' `header`).
    let tools_need = match studio.ws.dock {
        DockTab::Rack => 230.0,
        DockTab::PianoRoll => 360.0,
        DockTab::Mixer => 270.0,
        DockTab::Perform => 300.0,
    };
    let show_labels = !shown || head.w - labels_w - picker_w - 16.0 >= tools_need;
    for (tab, icon, label, key) in crate::layout::DOCK_TABS {
        let w = if show_labels {
            ui.tab_width(label)
        } else {
            32.0
        };
        let cell = head.cut_left(w);
        let on = shown && studio.ws.dock == tab;
        let resp = ui.tab(
            W::new(("dock-tab", label)),
            cell,
            icon,
            label,
            show_labels,
            on,
        );
        ui.describe_node(resp.id, peel_ui::Semantics::patch().shortcut(key));
        ui.tooltip(&resp, &format!("{label} ({key})"));
        if resp.clicked {
            crate::layout::dock_tab(studio, tab, true);
        }
    }
    if !shown {
        return;
    }
    // The pattern being edited sits with the editors that edit it.
    let mut tools = head.shrink2(0.0, 4.0);
    if has_picker {
        tools.cut_left(8.0);
        ui.divider_v(tools.cut_left(1.0));
        tools.cut_left(8.0);
        // The picker yields room before the tools do: down to 96 px with
        // its name, then to a swatch and caret (name in its tooltip).
        let room = tools.w - tools_need - 50.0;
        let w = if room >= 96.0 { room.min(160.0) } else { 44.0 };
        transport::pattern_picker(ui, &mut tools, studio, w);
        tools.cut_left(8.0);
    }
    let content = body.shrink2(0.0, 0.0);
    match studio.ws.dock {
        DockTab::Rack => rack::draw(ui, content, tools, &mut studio.session, &mut studio.ws),
        DockTab::PianoRoll => {
            piano_roll::draw(ui, content, tools, &mut studio.session, &mut studio.ws)
        }
        DockTab::Mixer => mixer::draw(ui, content, tools, &mut studio.session, &mut studio.ws),
        DockTab::Perform => {
            crate::perform::draw(ui, content, tools, &mut studio.session, &mut studio.ws)
        }
    }
}
