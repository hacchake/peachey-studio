//! Peachey Studio desktop application.

mod analysis_view;
mod audio_io;
mod browser;
mod dev_take;
mod device_layout;
mod dialogs;
mod eq_editor;
mod eq_resonance;
mod expression;
mod fx_editor;
mod harmony;
mod inspector;
mod instrument_editor;
mod keys;
mod layout;
mod mastering_ui;
mod menus;
mod midi;
mod mixer;
mod note_expr;
mod note_tools;
mod perform;
mod piano_roll;
mod playlist;
mod prefs;
mod project;
mod rack;
mod recorder;
mod rename;
mod script_host;
mod scripts;
mod settings;
mod studio;
mod transport;
mod user_templates;
mod util;
mod welcome;
mod workspace;

#[cfg(test)]
mod mpe_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
use inspector::reaches as inspector_reaches;

use std::path::PathBuf;

use peel_session::{Session, template};
use peel_ui::WindowOptions;

fn main() {
    if peel_extensions::dispatch_worker() {
        return;
    }

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info,wgpu_core=warn,wgpu_hal=warn,naga=warn"),
    )
    .init();
    let mut mcp = false;
    let mut headless = false;
    let mut open: Option<PathBuf> = None;
    for arg in std::env::args_os().skip(1) {
        if arg == "--mcp" {
            mcp = true;
        } else if arg == "--mcp-headless" {
            headless = true;
        } else if arg == "--help" || arg == "-h" {
            eprintln!("Usage: peachey-studio [--mcp | --mcp-headless] [project.peelproj]");
            return;
        } else if arg.to_string_lossy().starts_with('-') || open.is_some() {
            eprintln!("Unknown argument: {}", arg.to_string_lossy());
            std::process::exit(2);
        } else {
            open = Some(PathBuf::from(arg));
        }
    }
    if mcp && headless {
        eprintln!("Choose either --mcp or --mcp-headless");
        std::process::exit(2);
    }
    if headless {
        if let Err(error) = peel_mcp::run_headless(open, false) {
            eprintln!("Peel MCP: {error}");
            std::process::exit(1);
        }
        return;
    }
    let prefs = prefs::Prefs::load();
    let project = startup_project();
    // `PEEL_SILENT` (development): the software clock, no audio or MIDI
    // hardware, for screenshots that play the transport.
    let mut session = if std::env::var_os("PEEL_SILENT").is_some() {
        Session::new_silent(project)
    } else {
        Session::new_with_audio(project, prefs.audio_device.clone(), prefs.buffer_frames)
    };
    session.prefer_input_device(prefs.input_device.clone());
    if let Some(c) = prefs.input_channels() {
        session.set_input_channels(c);
    }
    let options = WindowOptions {
        title: "Peachey Studio".into(),
        // タスクバーやウィンドウの左上に出る桃のアイコン。
        icon_png: Some(include_bytes!("../../../assets/icons/peachey-256.png")),
        // A Windows native menu takes F10 and Alt from the editors.
        native_menus: cfg!(target_os = "macos"),
        ..Default::default()
    };
    let result = peel_ui::run(options, move |_ui| {
        let mut studio = studio::Studio::new(session);
        if let Some(path) = open {
            studio.open_path(path);
        }
        if mcp {
            studio.enable_mcp();
        }
        studio
    });
    if let Err(e) = result {
        eprintln!("Peachey Studio could not start: {e}");
        std::process::exit(1);
    }
}

/// Peachey Studio opens on a blank project. The demo songs are for
/// development: `PEEL_DEMO=1` asks for the first, `PEEL_DEMO=<title>` for
/// one by name, and screenshot runs (`PEEL_SCREENSHOT`) get the first
/// unless `PEEL_DEMO=0`, since their view hooks expect its channels.
fn startup_project() -> peel_document::Project {
    match std::env::var("PEEL_DEMO") {
        Ok(v) if v == "0" => template::empty(),
        Ok(v) => template::demos()
            .into_iter()
            .find(|d| d.title.eq_ignore_ascii_case(&v))
            .map_or_else(template::showcase, |d| (d.make)()),
        Err(_) if std::env::var_os("PEEL_SCREENSHOT").is_some() => template::showcase(),
        Err(_) => template::empty(),
    }
}
