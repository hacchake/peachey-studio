//! [`ClapHandle`]: the control-side face of a loaded plugin.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use peel_dsp::{DeviceDescriptor, ParamId};

use crate::error::ClapError;
use crate::info::ClapPluginInfo;
use crate::instance::{IdleReport, PluginShared};

/// Counters for diagnostics. Cheap to read from any thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ClapStats {
    /// Events dropped because the per-block event list was full.
    pub dropped_events: u32,
    /// Blocks where the plugin returned `CLAP_PROCESS_ERROR` (or failed to
    /// start processing).
    pub process_errors: u32,
    /// Log messages the plugin sent from the audio thread. They are counted,
    /// not printed, because logging is not real-time safe.
    pub audio_thread_logs: u32,
}

impl ClapStats {
    pub(crate) fn of(shared: &PluginShared) -> ClapStats {
        ClapStats {
            dropped_events: shared.dropped_events.load(Ordering::Relaxed),
            process_errors: shared.process_errors.load(Ordering::Relaxed),
            audio_thread_logs: shared.host.shared.audio_thread_logs.load(Ordering::Relaxed),
        }
    }
}

/// Control-side access to a plugin that has been moved into the engine.
///
/// `ClapDevice` is a `Box<dyn Device>` by the time the engine owns it, so the
/// application keeps one of these for everything that is not real time:
/// servicing the plugin's callbacks, saving and loading state, reading
/// parameter values and display text. It is `Clone + Send + Sync`.
///
/// Methods that call into the plugin are serialised by an internal lock, so
/// they are safe to call from more than one thread, one at a time. None of
/// them touches the audio thread's data except through atomics.
///
/// The plugin is destroyed when the device and every handle have been
/// dropped, on whichever thread drops the last one. Keep that off the audio
/// thread: the engine already returns removed devices to a control thread,
/// and handles should be dropped there too.
#[derive(Clone)]
pub struct ClapHandle {
    shared: Arc<PluginShared>,
    info: ClapPluginInfo,
    descriptor: &'static DeviceDescriptor,
}

impl ClapHandle {
    pub(crate) fn new(
        shared: Arc<PluginShared>,
        info: ClapPluginInfo,
        descriptor: &'static DeviceDescriptor,
    ) -> ClapHandle {
        ClapHandle {
            shared,
            info,
            descriptor,
        }
    }

    pub fn info(&self) -> &ClapPluginInfo {
        &self.info
    }

    pub fn descriptor(&self) -> &'static DeviceDescriptor {
        self.descriptor
    }

    /// Services the plugin's requests. **Call this every UI frame** (or on a
    /// timer of a few milliseconds) from the control side.
    ///
    /// It runs the plugin's `on_main_thread` when the plugin called
    /// `host->request_callback`, picks up latency, tail and parameter
    /// notifications, and reports what changed. It never deactivates or
    /// re-creates the plugin: when the plugin wants a restart the report says
    /// so and the application decides when to call `ClapDevice::prepare`.
    pub fn idle(&self) -> IdleReport {
        let report = self.shared.idle();
        crate::gui::idle(&self.shared);
        report
    }

    /// Whether the plugin can show its own window here (Windows, embedded
    /// win32 window).
    pub fn has_editor(&self) -> bool {
        crate::gui::available(&self.shared)
    }

    /// Whether the plugin's window is showing.
    pub fn editor_open(&self) -> bool {
        crate::gui::is_open(&self.shared)
    }

    /// Opens the plugin's own window, or brings it forward. Main thread.
    pub fn open_editor(&self) -> Result<(), String> {
        crate::gui::open(&self.shared, &self.info.name)
    }

    /// Closes the plugin's window. Main thread.
    pub fn close_editor(&self) {
        crate::gui::close(&self.shared);
    }

    /// The plugin's complete state as an opaque blob. The host stores it in
    /// the project and gives it back to [`load_state`](Self::load_state).
    /// Never call this from the audio thread. Fails with
    /// [`ClapError::Unsupported`] when the plugin has no state extension; the
    /// host does not invent a state from parameter values.
    pub fn save_state(&self) -> Result<Vec<u8>, ClapError> {
        self.shared.save_state()
    }

    /// Restores a blob from [`save_state`](Self::save_state). Do this before
    /// the device starts playing, or accept that `set_param` calls already
    /// queued on the audio side will be applied after the restore.
    pub fn load_state(&self, data: &[u8]) -> Result<(), ClapError> {
        self.shared.load_state(data)
    }

    pub fn can_save_state(&self) -> bool {
        self.shared.has_state()
    }

    pub fn has_gui(&self) -> bool {
        self.shared.has_gui()
    }

    /// Current plain value of a parameter: what the host last set, or what
    /// the plugin last reported. Lock-free.
    pub fn param(&self, id: ParamId) -> Option<f32> {
        self.shared
            .table
            .index_of(id)
            .and_then(|i| self.shared.value(i))
    }

    /// The plugin's own text for a value ("-6.0 dB", "Saw"). `None` when the
    /// plugin cannot format it.
    pub fn param_text(&self, id: ParamId, plain: f32) -> Option<String> {
        self.shared.param_text(id, f64::from(plain))
    }

    /// Parses text typed by the user into a plain value with the plugin's own
    /// parser.
    pub fn param_from_text(&self, id: ParamId, text: &str) -> Option<f32> {
        self.shared.param_from_text(id, text).map(|v| v as f32)
    }

    /// Latency in samples the plugin reports.
    pub fn latency(&self) -> u32 {
        self.shared.latency_frames()
    }

    /// Tail in seconds; `None` if unknown or infinite.
    pub fn tail_seconds(&self) -> Option<f32> {
        self.shared.tail_seconds()
    }

    /// True when the plugin declares it must run in real time and cannot be
    /// rendered faster than playback.
    pub fn requires_realtime(&self) -> bool {
        self.shared.requires_realtime()
    }

    pub fn stats(&self) -> ClapStats {
        ClapStats::of(&self.shared)
    }
}
