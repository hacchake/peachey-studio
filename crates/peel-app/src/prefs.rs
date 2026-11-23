//! User preferences and recent projects, stored as JSON in the platform
//! config directory. Never part of a project.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::workspace::{BrowserTab, DockTab, Workspace};

const MAX_RECENT: usize = 12;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub recent: Vec<PathBuf>,
    pub show_welcome: bool,
    pub show_browser: bool,
    pub show_inspector: bool,
    pub show_scripts: bool,
    pub show_editor: bool,
    pub show_help: bool,
    /// Master meter and load in the top bar.
    pub show_meter: bool,
    /// Device, sample rate and octave in the status bar.
    pub show_status_details: bool,
    /// Pane sizes and tabs as the user last left them.
    pub browser_w: Option<f32>,
    pub inspector_w: Option<f32>,
    pub dock_h: Option<f32>,
    pub dock_tab: Option<DockTab>,
    pub browser_tab: Option<BrowserTab>,
    pub audio_device: Option<String>,
    pub buffer_frames: Option<u32>,
    pub octave: i32,
    pub sample_dir: Option<PathBuf>,
    /// Optional virtual environment interpreter for Python actions.
    pub python: Option<PathBuf>,
    /// Audio input to record from (`None` is the system default).
    pub input_device: Option<String>,
    /// Device channels recorded: one (mono) or two (stereo), zero-based.
    pub input_channels: Vec<u16>,
    /// Controller (CC) mappings; applied to each project that opens, and
    /// the ones that do not fit it are reported and dropped.
    pub midi_mappings: Vec<peel_session::MidiMapping>,
    /// Only listen to this MIDI channel (0..15); `None` listens to all.
    pub midi_channel: Option<u8>,
    /// Pitch wheel range in semitones (machine level).
    pub pitch_bend_range: f32,
    /// MPE zone for incoming MIDI (machine setting, off by default).
    pub mpe: peel_session::MpeConfig,
    /// Secondary hardware outputs (machine level, never in a project).
    pub output_routes: Vec<peel_session::OutputRoute>,
    /// Channels the main output uses on its device.
    pub primary_output: Option<peel_session::OutputChannels>,
    /// Folder for new recordings (`None` keeps the default next to the
    /// project).
    pub recordings_dir: Option<PathBuf>,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            recent: Vec::new(),
            show_welcome: true,
            show_browser: true,
            show_inspector: true,
            show_scripts: false,
            show_editor: true,
            show_help: false,
            show_meter: true,
            show_status_details: true,
            browser_w: None,
            inspector_w: None,
            dock_h: None,
            dock_tab: None,
            browser_tab: None,
            audio_device: None,
            buffer_frames: None,
            octave: 4,
            sample_dir: None,
            python: None,
            input_device: None,
            input_channels: Vec::new(),
            midi_mappings: Vec::new(),
            midi_channel: None,
            pitch_bend_range: 2.0,
            mpe: peel_session::MpeConfig::default(),
            output_routes: Vec::new(),
            primary_output: None,
            recordings_dir: None,
        }
    }
}

/// `<config>/Peel Studio` (macOS: ~/Library/Application Support).
///
/// Peachey Studio に名前を変えた後も、このフォルダ名は変えない。設定・自動
/// 保存・スクリプトをそのまま引き継ぎ、保存前の録音への参照も切らさないため。
pub fn app_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("Peel Studio")
}

pub fn autosave_dir() -> PathBuf {
    app_dir().join("Autosave")
}

fn prefs_path() -> PathBuf {
    app_dir().join("prefs.json")
}

impl Prefs {
    /// Loads preferences; tests and screenshots use defaults when
    /// `PEEL_NO_PREFS` is set.
    pub fn load() -> Prefs {
        if std::env::var_os("PEEL_NO_PREFS").is_some() || cfg!(test) {
            return Prefs::default();
        }
        std::fs::read(prefs_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        if std::env::var_os("PEEL_NO_PREFS").is_some() || cfg!(test) {
            return;
        }
        let path = prefs_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }

    pub fn add_recent(&mut self, path: &Path) {
        let path = path.to_path_buf();
        self.recent.retain(|p| p != &path);
        self.recent.insert(0, path);
        self.recent.truncate(MAX_RECENT);
        self.save();
    }

    /// Recent projects that still exist on disk.
    pub fn existing_recent(&self) -> Vec<PathBuf> {
        self.recent.iter().filter(|p| p.exists()).cloned().collect()
    }
}

impl Prefs {
    /// The saved channel choice as the session's type.
    pub fn input_channels(&self) -> Option<peel_session::InputChannels> {
        match self.input_channels.as_slice() {
            [a] => Some(peel_session::InputChannels::Mono(*a)),
            [a, b] => Some(peel_session::InputChannels::Stereo(*a, *b)),
            _ => None,
        }
    }

    pub fn set_input_channels(&mut self, c: peel_session::InputChannels) {
        self.input_channels = match c {
            peel_session::InputChannels::Mono(a) => vec![a],
            peel_session::InputChannels::Stereo(a, b) => vec![a, b],
        };
    }
}

impl Prefs {
    /// The right column shows the Inspector or Scripts (never both).
    pub fn show_right(&self) -> bool {
        self.show_inspector || self.show_scripts
    }

    /// Shows the Inspector in the right column, or hides the column.
    pub fn set_inspector(&mut self, on: bool) {
        self.show_inspector = on;
        if on {
            self.show_scripts = false;
        }
    }

    /// Shows Scripts in the right column, or hides the column.
    pub fn set_scripts(&mut self, on: bool) {
        self.show_scripts = on;
        if on {
            self.show_inspector = false;
        }
    }

    /// Restores the saved pane sizes and tabs into a fresh workspace.
    pub fn apply_layout(&self, ws: &mut Workspace) {
        if let Some(w) = self.browser_w {
            ws.browser_w = w;
        }
        if let Some(w) = self.inspector_w {
            ws.inspector_w = w;
        }
        if let Some(h) = self.dock_h {
            ws.dock_h = h;
        }
        if let Some(t) = self.dock_tab {
            ws.dock = t;
        }
        if let Some(t) = self.browser_tab {
            ws.browser_tab = t;
        }
    }

    /// Copies the workspace's pane sizes and tabs; true when any changed.
    pub fn capture_layout(&mut self, ws: &Workspace) -> bool {
        let next = (
            Some(ws.browser_w.round()),
            Some(ws.inspector_w.round()),
            Some(ws.dock_h.round()),
            Some(ws.dock),
            Some(ws.browser_tab),
        );
        let now = (
            self.browser_w,
            self.inspector_w,
            self.dock_h,
            self.dock_tab,
            self.browser_tab,
        );
        if next == now {
            return false;
        }
        (
            self.browser_w,
            self.inspector_w,
            self.dock_h,
            self.dock_tab,
            self.browser_tab,
        ) = next;
        true
    }

    /// Puts every pane, size and toggle back to its default.
    pub fn reset_layout(&mut self) {
        let d = Prefs::default();
        self.show_browser = d.show_browser;
        self.show_inspector = d.show_inspector;
        self.show_scripts = d.show_scripts;
        self.show_editor = d.show_editor;
        self.show_help = d.show_help;
        self.show_meter = d.show_meter;
        self.show_status_details = d.show_status_details;
        self.browser_w = None;
        self.inspector_w = None;
        self.dock_h = None;
        self.dock_tab = None;
        self.browser_tab = None;
    }
}
