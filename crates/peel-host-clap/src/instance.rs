//! One created CLAP plugin instance and everything both sides share about it.
//!
//! CLAP splits an instance's functions between the *main thread* and the
//! *audio thread* and allows the two to run at the same time. This type
//! models that split:
//!
//! - main-thread functions (activate, state, parameter text, `idle`) take the
//!   `main` mutex, so callers on different threads are serialised, and run
//!   inside a main-thread role scope
//! - audio-thread functions (`process`, `reset`, start and stop processing)
//!   take no lock and are only reached through [`crate::ClapDevice`]
//!
//! The parameter mirror and the latency/tail cache are atomics, so the
//! control side can read them while the audio side writes.

use std::ffi::{CStr, CString, c_char};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use clap_sys::events::{clap_input_events, clap_output_events};
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports,
};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_plugin_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_ports::{CLAP_EXT_NOTE_PORTS, clap_note_port_info, clap_plugin_note_ports};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_IS_HIDDEN, CLAP_PARAM_IS_READONLY, CLAP_PARAM_IS_STEPPED,
    CLAP_PARAM_RESCAN_ALL, CLAP_PARAM_RESCAN_INFO, CLAP_PARAM_RESCAN_TEXT,
    CLAP_PARAM_RESCAN_VALUES, clap_param_info, clap_plugin_params,
};
use clap_sys::ext::render::{
    CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME, clap_plugin_render,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_plugin_tail};
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{clap_process, clap_process_status};

use crate::error::ClapError;
use crate::host::{
    F_CALLBACK, F_LATENCY, F_PARAMS_RESCAN, F_PORTS_RESCAN, F_RESTART, F_STATE_DIRTY, F_TAIL,
    HostBox, MAIN_THREAD_FLAGS,
};
use crate::module::ModuleHandle;
use crate::params::{ParamEntry, ParamTable};
use crate::state::{MAX_STATE_BYTES, StateReader, StateWriter};
use crate::thread::RoleScope;

/// Ports and parameters beyond these are ignored. They exist to bound memory
/// when a plugin reports nonsense.
const MAX_PORTS: u32 = 64;
const MAX_PARAMS: u32 = 16_384;
const MAX_CHANNELS: u32 = 64;

/// `tail` cache value for "unknown or infinite".
pub(crate) const TAIL_UNBOUNDED: u32 = u32::MAX;

#[derive(Clone, Debug)]
pub(crate) struct PortInfo {
    pub(crate) channels: u32,
    pub(crate) is_main: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct NotePortInfo {
    pub(crate) supported_dialects: u32,
}

/// A parameter exactly as the plugin reported it.
#[derive(Clone, Debug)]
pub(crate) struct RawParam {
    pub(crate) id: u32,
    pub(crate) flags: u32,
    pub(crate) cookie: usize,
    pub(crate) name: String,
    pub(crate) module: String,
    pub(crate) min: f64,
    pub(crate) max: f64,
    pub(crate) default: f64,
}

impl RawParam {
    pub(crate) fn is_stepped(&self) -> bool {
        self.flags & CLAP_PARAM_IS_STEPPED != 0
    }

    /// Hidden and read-only parameters are not offered for editing or
    /// automation.
    fn is_exposed(&self) -> bool {
        self.flags & (CLAP_PARAM_IS_HIDDEN | CLAP_PARAM_IS_READONLY) == 0
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Extensions {
    params: *const clap_plugin_params,
    state: *const clap_plugin_state,
    latency: *const clap_plugin_latency,
    tail: *const clap_plugin_tail,
    audio_ports: *const clap_plugin_audio_ports,
    note_ports: *const clap_plugin_note_ports,
    pub(crate) gui: *const clap_plugin_gui,
    render: *const clap_plugin_render,
}

impl Extensions {
    const NONE: Extensions = Extensions {
        params: std::ptr::null(),
        state: std::ptr::null(),
        latency: std::ptr::null(),
        tail: std::ptr::null(),
        audio_ports: std::ptr::null(),
        note_ports: std::ptr::null(),
        gui: std::ptr::null(),
        render: std::ptr::null(),
    };
}

pub(crate) struct MainState {
    active: bool,
}

/// What `idle()` found. Every field is "since the last `idle()`" except
/// `restart_requested`, which stays set until the device is prepared again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdleReport {
    /// The plugin's `on_main_thread` ran because it asked for a callback.
    pub callback_serviced: bool,
    /// The plugin needs to be deactivated and activated again (changed
    /// latency, ports or parameters). Call `ClapDevice::prepare` while the
    /// device is off the audio thread. Reported on every `idle()` until then.
    pub restart_requested: bool,
    /// Parameter values or display text changed outside `set_param`; read
    /// them again with `ClapHandle::param`.
    pub params_changed: bool,
    /// The parameter list itself changed. The `DeviceDescriptor` is fixed
    /// for the life of the device, so load a fresh `ClapDevice`.
    pub param_layout_changed: bool,
    /// Audio or note ports changed. Load a fresh `ClapDevice`.
    pub ports_changed: bool,
    pub latency_changed: bool,
    pub tail_changed: bool,
    /// The plugin has unsaved changes: save its state again.
    pub state_dirty: bool,
}

/// An initialised plugin instance plus its host.
pub(crate) struct PluginShared {
    pub(crate) plugin: *const clap_plugin,
    pub(crate) exts: Extensions,
    pub(crate) plugin_id: String,
    pub(crate) audio_in: Vec<PortInfo>,
    pub(crate) audio_out: Vec<PortInfo>,
    pub(crate) note_in: Vec<NotePortInfo>,
    pub(crate) note_out_count: u32,
    /// Parameters offered to the application, in plugin order.
    pub(crate) exposed: Vec<RawParam>,
    pub(crate) table: ParamTable,
    /// Current plain value of each exposed parameter (`f32` bits), kept
    /// current by `set_param` and by the plugin's own output events.
    values: Box<[AtomicU32]>,
    main: Mutex<MainState>,
    /// The plugin's own window, when open (main thread only).
    pub(crate) editor: Mutex<Option<crate::gui::ClapEditor>>,
    latency: AtomicU32,
    tail: AtomicU32,
    sample_rate: AtomicU32,
    pub(crate) dropped_events: AtomicU32,
    pub(crate) process_errors: AtomicU32,
    pub(crate) host: Box<HostBox>,
    // Declared last: the library must outlive the plugin and its host.
    _module: ModuleHandle,
}

// SAFETY: the raw pointers are the plugin instance and its extension tables.
// CLAP lets the main thread and the audio thread call into one instance
// concurrently, and this type keeps each kind of call on its own path (a
// mutex on one, nothing shared but atomics on the other).
unsafe impl Send for PluginShared {}
unsafe impl Sync for PluginShared {}

fn c_array_string(chars: &[c_char]) -> String {
    let bytes: Vec<u8> = chars
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c as u8)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

impl PluginShared {
    /// Creates and initialises the plugin `id` from `module`. The instance
    /// is left inactive: call [`PluginShared::activate`] before processing.
    pub(crate) fn open(
        module: ModuleHandle,
        id: &str,
        label: &str,
    ) -> Result<PluginShared, ClapError> {
        let c_id = CString::new(id)
            .map_err(|_| ClapError::InvalidArgument("plugin id contains a NUL".into()))?;
        let host = HostBox::new(label);
        let plugin = module.create_plugin(host.raw_ptr(), &c_id);
        if plugin.is_null() {
            return Err(ClapError::PluginNotFound {
                path: module.path().to_path_buf(),
                id: id.to_string(),
            });
        }
        // SAFETY: just created by the factory, valid until `destroy`.
        let p = unsafe { &*plugin };
        let mut shared = PluginShared {
            plugin,
            exts: Extensions::NONE,
            plugin_id: id.to_string(),
            audio_in: Vec::new(),
            audio_out: Vec::new(),
            note_in: Vec::new(),
            note_out_count: 0,
            exposed: Vec::new(),
            table: ParamTable::new(Vec::new()),
            values: Box::new([]),
            main: Mutex::new(MainState { active: false }),
            editor: Mutex::new(None),
            latency: AtomicU32::new(0),
            tail: AtomicU32::new(TAIL_UNBOUNDED),
            sample_rate: AtomicU32::new(0),
            dropped_events: AtomicU32::new(0),
            process_errors: AtomicU32::new(0),
            host,
            _module: module,
        };
        // From here a failure drops `shared`, whose `Drop` calls `destroy`.
        if p.destroy.is_none() {
            // Without `destroy` the instance cannot be released at all.
            std::mem::forget(shared);
            return Err(ClapError::MissingEntryPoint {
                id: id.to_string(),
                entry: "destroy",
            });
        }
        for (present, entry) in [
            (p.init.is_some(), "init"),
            (p.activate.is_some(), "activate"),
            (p.deactivate.is_some(), "deactivate"),
            (p.process.is_some(), "process"),
            (p.get_extension.is_some(), "get_extension"),
        ] {
            if !present {
                return Err(ClapError::MissingEntryPoint {
                    id: id.to_string(),
                    entry,
                });
            }
        }
        {
            let _main = RoleScope::main();
            let init = p.init.ok_or_else(|| ClapError::MissingEntryPoint {
                id: id.to_string(),
                entry: "init",
            })?;
            // SAFETY: `plugin` is a freshly created instance.
            if !unsafe { init(plugin) } {
                return Err(ClapError::PluginInitFailed { id: id.to_string() });
            }
            shared.query_extensions();
            shared.query_ports();
            shared.query_params();
        }
        Ok(shared)
    }

    fn plugin_ref(&self) -> &clap_plugin {
        // SAFETY: valid from creation until `Drop` calls `destroy`.
        unsafe { &*self.plugin }
    }

    fn ext<T>(&self, id: &CStr) -> *const T {
        match self.plugin_ref().get_extension {
            // SAFETY: `id` is a valid C string and the plugin is initialised.
            Some(get) => unsafe { get(self.plugin, id.as_ptr()) }.cast::<T>(),
            None => std::ptr::null(),
        }
    }

    fn query_extensions(&mut self) {
        self.exts = Extensions {
            params: self.ext(CLAP_EXT_PARAMS),
            state: self.ext(CLAP_EXT_STATE),
            latency: self.ext(CLAP_EXT_LATENCY),
            tail: self.ext(CLAP_EXT_TAIL),
            audio_ports: self.ext(CLAP_EXT_AUDIO_PORTS),
            note_ports: self.ext(CLAP_EXT_NOTE_PORTS),
            gui: self.ext(CLAP_EXT_GUI),
            render: self.ext(CLAP_EXT_RENDER),
        };
    }

    fn query_ports(&mut self) {
        self.audio_in = self.audio_ports(true);
        self.audio_out = self.audio_ports(false);
        self.note_in = self.note_ports(true);
        self.note_out_count = self.note_ports(false).len() as u32;
    }

    fn audio_ports(&self, is_input: bool) -> Vec<PortInfo> {
        // SAFETY: the extension pointer is null or valid while the plugin is.
        let Some(ext) = (unsafe { self.exts.audio_ports.as_ref() }) else {
            return Vec::new();
        };
        let (Some(count), Some(get)) = (ext.count, ext.get) else {
            return Vec::new();
        };
        // SAFETY: plugin is initialised; main-thread calls.
        let n = unsafe { count(self.plugin, is_input) }.min(MAX_PORTS);
        let mut ports = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: all-zero is valid for this plain C struct.
            let mut info: clap_audio_port_info = unsafe { std::mem::zeroed() };
            // SAFETY: `info` is a valid out pointer.
            if unsafe { get(self.plugin, i, is_input, &mut info) } {
                ports.push(PortInfo {
                    channels: info.channel_count.clamp(1, MAX_CHANNELS),
                    is_main: info.flags & CLAP_AUDIO_PORT_IS_MAIN != 0,
                });
            }
        }
        ports
    }

    fn note_ports(&self, is_input: bool) -> Vec<NotePortInfo> {
        // SAFETY: as in `audio_ports`.
        let Some(ext) = (unsafe { self.exts.note_ports.as_ref() }) else {
            return Vec::new();
        };
        let (Some(count), Some(get)) = (ext.count, ext.get) else {
            return Vec::new();
        };
        // SAFETY: plugin is initialised; main-thread calls.
        let n = unsafe { count(self.plugin, is_input) }.min(MAX_PORTS);
        let mut ports = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: all-zero is valid for this plain C struct.
            let mut info: clap_note_port_info = unsafe { std::mem::zeroed() };
            // SAFETY: `info` is a valid out pointer.
            if unsafe { get(self.plugin, i, is_input, &mut info) } {
                ports.push(NotePortInfo {
                    supported_dialects: info.supported_dialects,
                });
            }
        }
        ports
    }

    fn query_params(&mut self) {
        // SAFETY: the extension pointer is null or valid while the plugin is.
        let Some(ext) = (unsafe { self.exts.params.as_ref() }) else {
            return;
        };
        let (Some(count), Some(get_info)) = (ext.count, ext.get_info) else {
            return;
        };
        // SAFETY: plugin is initialised; main-thread calls.
        let n = unsafe { count(self.plugin) }.min(MAX_PARAMS);
        let mut exposed: Vec<RawParam> = Vec::with_capacity(n as usize);
        for i in 0..n {
            // SAFETY: all-zero is valid for this plain C struct.
            let mut info: clap_param_info = unsafe { std::mem::zeroed() };
            // SAFETY: `info` is a valid out pointer.
            if !unsafe { get_info(self.plugin, i, &mut info) } {
                continue;
            }
            let param = RawParam {
                id: info.id,
                flags: info.flags,
                cookie: info.cookie as usize,
                name: c_array_string(&info.name),
                module: c_array_string(&info.module),
                min: info.min_value,
                max: info.max_value,
                default: info.default_value,
            };
            let finite = param.min.is_finite() && param.max.is_finite() && param.min <= param.max;
            if param.is_exposed() && finite && !exposed.iter().any(|p| p.id == param.id) {
                exposed.push(param);
            }
        }
        self.table = ParamTable::new(
            exposed
                .iter()
                .map(|p| ParamEntry {
                    id: p.id,
                    cookie: p.cookie,
                    min: p.min,
                    max: p.max,
                    stepped: p.is_stepped(),
                })
                .collect(),
        );
        let initial: Vec<AtomicU32> = exposed
            .iter()
            .map(|p| {
                let v = self.get_value_unlocked(p.id).unwrap_or(p.default);
                AtomicU32::new((v as f32).to_bits())
            })
            .collect();
        self.values = initial.into_boxed_slice();
        self.exposed = exposed;
    }

    pub(crate) fn lock_main(&self) -> MutexGuard<'_, MainState> {
        self.main.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Like [`lock_main`](Self::lock_main), but gives up when the lock is
    /// held (window procedures must not wait on it).
    pub(crate) fn try_lock_main(&self) -> Result<MutexGuard<'_, MainState>, ()> {
        match self.main.try_lock() {
            Ok(g) => Ok(g),
            Err(std::sync::TryLockError::Poisoned(p)) => Ok(p.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => Err(()),
        }
    }

    // ---- extension presence --------------------------------------------

    pub(crate) fn has_gui(&self) -> bool {
        !self.exts.gui.is_null()
    }

    pub(crate) fn has_state(&self) -> bool {
        !self.exts.state.is_null()
    }

    // ---- main thread: lifecycle ----------------------------------------

    /// Deactivates if needed, then activates for `sample_rate` and blocks of
    /// at most `max_block` frames. The caller guarantees the audio thread is
    /// not inside `process`.
    pub(crate) fn activate(&self, sample_rate: f32, max_block: usize) -> Result<(), ClapError> {
        let mut main = self.lock_main();
        let _scope = RoleScope::main();
        let p = self.plugin_ref();
        let (Some(activate), Some(deactivate)) = (p.activate, p.deactivate) else {
            return Err(ClapError::MissingEntryPoint {
                id: self.plugin_id.clone(),
                entry: "activate",
            });
        };
        if main.active {
            // SAFETY: active and not processing (caller's guarantee).
            unsafe { deactivate(self.plugin) };
            main.active = false;
        }
        let frames = u32::try_from(max_block).unwrap_or(u32::MAX);
        // SAFETY: the plugin is initialised and inactive.
        if !unsafe { activate(self.plugin, f64::from(sample_rate), 1, frames) } {
            return Err(ClapError::ActivateFailed {
                id: self.plugin_id.clone(),
                sample_rate,
                max_block,
            });
        }
        main.active = true;
        self.sample_rate
            .store(sample_rate.to_bits(), Ordering::Relaxed);
        self.read_latency_and_tail();
        self.host.shared.take(F_RESTART);
        self.refresh_values_unlocked();
        Ok(())
    }

    // ---- audio thread ----------------------------------------------------

    pub(crate) fn start_processing(&self) -> bool {
        match self.plugin_ref().start_processing {
            // SAFETY: active and not processing; audio-thread call.
            Some(f) => unsafe { f(self.plugin) },
            None => true,
        }
    }

    pub(crate) fn stop_processing(&self) {
        if let Some(f) = self.plugin_ref().stop_processing {
            // SAFETY: active and processing; audio-thread call.
            unsafe { f(self.plugin) };
        }
    }

    pub(crate) fn reset(&self) {
        if let Some(f) = self.plugin_ref().reset {
            // SAFETY: active; audio-thread call.
            unsafe { f(self.plugin) };
        }
    }

    pub(crate) fn process(&self, process: &clap_process) -> clap_process_status {
        match self.plugin_ref().process {
            // SAFETY: `process` and everything it points to are valid for
            // the call; audio-thread call on an active, started plugin.
            Some(f) => unsafe { f(self.plugin, process) },
            None => clap_sys::process::CLAP_PROCESS_ERROR,
        }
    }

    /// True when the plugin can take parameter changes while not processing.
    pub(crate) fn has_flush(&self) -> bool {
        // SAFETY: extension pointer is null or valid while the plugin is.
        unsafe { self.exts.params.as_ref() }.is_some_and(|p| p.flush.is_some())
    }

    /// Delivers parameter events without a `process` call. Audio-thread
    /// function: the caller must not be inside `process`.
    pub(crate) fn flush(&self, input: &clap_input_events, output: &clap_output_events) {
        // SAFETY: extension pointer is null or valid while the plugin is.
        if let Some(flush) = unsafe { self.exts.params.as_ref() }.and_then(|p| p.flush) {
            // SAFETY: both lists are valid for the call.
            unsafe { flush(self.plugin, input, output) };
        }
    }

    /// Records a value the plugin changed itself (output event).
    pub(crate) fn apply_plugin_param(&self, id: u32, value: f64) {
        if let Some(index) = self.table.index_of(id)
            && let Some(slot) = self.values.get(index)
        {
            slot.store((value as f32).to_bits(), Ordering::Relaxed);
        }
    }

    pub(crate) fn store_value(&self, index: usize, value: f64) {
        if let Some(slot) = self.values.get(index) {
            slot.store((value as f32).to_bits(), Ordering::Relaxed);
        }
    }

    pub(crate) fn value(&self, index: usize) -> Option<f32> {
        self.values
            .get(index)
            .map(|v| f32::from_bits(v.load(Ordering::Relaxed)))
    }

    // ---- latency and tail ------------------------------------------------

    fn read_latency_and_tail(&self) {
        // SAFETY: extension pointers are null or valid while the plugin is.
        if let Some(get) = unsafe { self.exts.latency.as_ref() }.and_then(|e| e.get) {
            // SAFETY: main-thread call on an active plugin.
            let frames = unsafe { get(self.plugin) };
            self.latency.store(frames, Ordering::Relaxed);
        }
        let tail = unsafe { self.exts.tail.as_ref() }
            .and_then(|e| e.get)
            // SAFETY: allowed on the main and audio threads.
            .map_or(TAIL_UNBOUNDED, |get| unsafe { get(self.plugin) });
        self.tail.store(tail, Ordering::Relaxed);
    }

    pub(crate) fn latency_frames(&self) -> u32 {
        self.latency.load(Ordering::Relaxed)
    }

    /// Tail in seconds. `None` when the plugin has no `tail` extension or
    /// reports an infinite tail.
    pub(crate) fn tail_seconds(&self) -> Option<f32> {
        let frames = self.tail.load(Ordering::Relaxed);
        let rate = f32::from_bits(self.sample_rate.load(Ordering::Relaxed));
        (frames != TAIL_UNBOUNDED && rate > 0.0).then(|| frames as f32 / rate)
    }

    // ---- main thread: parameters ------------------------------------------

    fn get_value_unlocked(&self, id: u32) -> Option<f64> {
        // SAFETY: extension pointer is null or valid while the plugin is.
        let get = unsafe { self.exts.params.as_ref() }?.get_value?;
        let mut out = 0.0f64;
        // SAFETY: valid out pointer; main-thread call.
        unsafe { get(self.plugin, id, &mut out) }.then_some(out)
    }

    fn refresh_values_unlocked(&self) {
        for (i, p) in self.exposed.iter().enumerate() {
            if let Some(v) = self.get_value_unlocked(p.id) {
                self.store_value(i, v);
            }
        }
    }

    /// Display text for `value`, using the plugin's own formatting.
    pub(crate) fn param_text(&self, id: u32, value: f64) -> Option<String> {
        let _guard = self.lock_main();
        let _scope = RoleScope::main();
        self.param_text_unlocked(id, value)
    }

    /// As [`param_text`](Self::param_text) for callers that already hold the
    /// main-thread role (descriptor construction during `open`).
    pub(crate) fn param_text_unlocked(&self, id: u32, value: f64) -> Option<String> {
        // SAFETY: extension pointer is null or valid while the plugin is.
        let to_text = unsafe { self.exts.params.as_ref() }?.value_to_text?;
        let mut buf = [0 as c_char; 256];
        // SAFETY: `buf` is writable for the stated capacity.
        let ok = unsafe { to_text(self.plugin, id, value, buf.as_mut_ptr(), buf.len() as u32) };
        ok.then(|| c_array_string(&buf)).filter(|s| !s.is_empty())
    }

    /// Parses `text` with the plugin's own parser.
    pub(crate) fn param_from_text(&self, id: u32, text: &str) -> Option<f64> {
        let _guard = self.lock_main();
        let _scope = RoleScope::main();
        // SAFETY: extension pointer is null or valid while the plugin is.
        let from_text = unsafe { self.exts.params.as_ref() }?.text_to_value?;
        let c_text = CString::new(text).ok()?;
        let mut out = 0.0f64;
        // SAFETY: valid C string and out pointer; main-thread call.
        unsafe { from_text(self.plugin, id, c_text.as_ptr(), &mut out) }.then_some(out)
    }

    // ---- main thread: state ----------------------------------------------

    pub(crate) fn save_state(&self) -> Result<Vec<u8>, ClapError> {
        // SAFETY: extension pointer is null or valid while the plugin is.
        let Some(save) = (unsafe { self.exts.state.as_ref() }).and_then(|e| e.save) else {
            return Err(ClapError::Unsupported(
                "the plugin has no state extension, so its state cannot be saved".into(),
            ));
        };
        let _guard = self.lock_main();
        let _scope = RoleScope::main();
        let mut writer = StateWriter::new();
        let stream = writer.stream();
        // SAFETY: `stream` points at `writer`, which outlives the call.
        let ok = unsafe { save(self.plugin, &stream) };
        if writer.overflowed {
            return Err(ClapError::State(format!(
                "the plugin wrote more than {MAX_STATE_BYTES} bytes"
            )));
        }
        if !ok {
            return Err(ClapError::State(
                "the plugin failed to save its state".into(),
            ));
        }
        Ok(writer.bytes)
    }

    pub(crate) fn load_state(&self, data: &[u8]) -> Result<(), ClapError> {
        // SAFETY: extension pointer is null or valid while the plugin is.
        let Some(load) = (unsafe { self.exts.state.as_ref() }).and_then(|e| e.load) else {
            return Err(ClapError::Unsupported(
                "the plugin has no state extension, so its state cannot be restored".into(),
            ));
        };
        let _guard = self.lock_main();
        let _scope = RoleScope::main();
        let mut reader = StateReader::new(data);
        let stream = reader.stream();
        // SAFETY: `stream` points at `reader`, which outlives the call.
        let ok = unsafe { load(self.plugin, &stream) };
        if !ok {
            return Err(ClapError::State(
                "the plugin rejected the saved state".into(),
            ));
        }
        self.refresh_values_unlocked();
        self.read_latency_and_tail();
        Ok(())
    }

    // ---- main thread: render mode -----------------------------------------

    pub(crate) fn set_render_offline(&self, offline: bool) -> bool {
        // SAFETY: extension pointer is null or valid while the plugin is.
        let Some(set) = (unsafe { self.exts.render.as_ref() }).and_then(|e| e.set) else {
            return false;
        };
        let _guard = self.lock_main();
        let _scope = RoleScope::main();
        let mode = if offline {
            CLAP_RENDER_OFFLINE
        } else {
            CLAP_RENDER_REALTIME
        };
        // SAFETY: main-thread call.
        unsafe { set(self.plugin, mode) }
    }

    pub(crate) fn requires_realtime(&self) -> bool {
        // SAFETY: extension pointer is null or valid while the plugin is.
        let Some(query) =
            (unsafe { self.exts.render.as_ref() }).and_then(|e| e.has_hard_realtime_requirement)
        else {
            return false;
        };
        let _guard = self.lock_main();
        let _scope = RoleScope::main();
        // SAFETY: main-thread call.
        unsafe { query(self.plugin) }
    }

    // ---- main thread: servicing -------------------------------------------

    pub(crate) fn idle(&self) -> IdleReport {
        let _guard = self.lock_main();
        let _scope = RoleScope::main();
        let host = &self.host.shared;
        let mut report = IdleReport::default();

        if host.take(F_CALLBACK) != 0 {
            if let Some(f) = self.plugin_ref().on_main_thread {
                // SAFETY: main-thread call on an initialised plugin.
                unsafe { f(self.plugin) };
            }
            report.callback_serviced = true;
        }

        let flags = host.take(MAIN_THREAD_FLAGS & !F_CALLBACK);
        if flags & F_LATENCY != 0 {
            let before = self.latency_frames();
            self.read_latency_and_tail();
            report.latency_changed = self.latency_frames() != before;
        }
        if flags & F_TAIL != 0 {
            self.read_latency_and_tail();
            report.tail_changed = true;
        }
        if flags & F_PARAMS_RESCAN != 0 {
            let rescan = host.params_rescan.swap(0, Ordering::AcqRel);
            if rescan & (CLAP_PARAM_RESCAN_VALUES | CLAP_PARAM_RESCAN_TEXT) != 0 {
                self.refresh_values_unlocked();
                report.params_changed = true;
            }
            if rescan & (CLAP_PARAM_RESCAN_INFO | CLAP_PARAM_RESCAN_ALL) != 0 {
                self.refresh_values_unlocked();
                report.params_changed = true;
                report.param_layout_changed = true;
            }
        }
        report.ports_changed = flags & F_PORTS_RESCAN != 0;
        report.state_dirty = flags & F_STATE_DIRTY != 0;
        report.restart_requested = host.is_raised(F_RESTART);
        report
    }
}

impl Drop for PluginShared {
    fn drop(&mut self) {
        let _scope = RoleScope::main();
        // The window must let go of the plugin before the plugin goes.
        if let Some(editor) = self
            .editor
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            crate::gui::destroy_unlocked(self, editor);
        }
        let active = self
            .main
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .active;
        let p = self.plugin_ref();
        if active && let Some(deactivate) = p.deactivate {
            // SAFETY: active; the owning device already stopped processing.
            unsafe { deactivate(self.plugin) };
        }
        if let Some(destroy) = p.destroy {
            // SAFETY: the last use of the instance. `host` and `module`
            // drop afterwards, in field order.
            unsafe { destroy(self.plugin) };
        }
    }
}
