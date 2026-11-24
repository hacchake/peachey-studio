//! The `clap_host` handed to each plugin and the host extensions behind it.
//!
//! Plugins call back from arbitrary threads, so every callback only touches
//! atomics. Requests are recorded as bits in [`HostShared::flags`]; the
//! control side collects the main-thread ones in `idle()`, and the audio side
//! checks the processing ones when deciding whether a sleeping device must
//! run.

use std::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_host_audio_ports};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency};
use clap_sys::ext::log::{
    CLAP_EXT_LOG, CLAP_LOG_DEBUG, CLAP_LOG_ERROR, CLAP_LOG_FATAL, CLAP_LOG_HOST_MISBEHAVING,
    CLAP_LOG_INFO, CLAP_LOG_PLUGIN_MISBEHAVING, CLAP_LOG_WARNING, clap_host_log, clap_log_severity,
};
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, clap_host_note_ports,
    clap_note_dialect,
};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_host_params, clap_param_clear_flags};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_host_tail};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::host::clap_host;
use clap_sys::version::CLAP_VERSION;

use crate::thread::{self, Role};

/// The plugin asked to be deactivated and activated again (new layout,
/// latency, parameter set). Level-triggered: stays set until `prepare`.
pub(crate) const F_RESTART: u32 = 1 << 0;
/// The plugin wants `on_main_thread` called.
pub(crate) const F_CALLBACK: u32 = 1 << 1;
/// The plugin wants `process` called even without input (audio side).
pub(crate) const F_PROCESS: u32 = 1 << 2;
/// The plugin wants a parameter flush, which a `process` call satisfies
/// (audio side).
pub(crate) const F_FLUSH: u32 = 1 << 3;
pub(crate) const F_LATENCY: u32 = 1 << 4;
pub(crate) const F_TAIL: u32 = 1 << 5;
pub(crate) const F_STATE_DIRTY: u32 = 1 << 6;
/// `params_rescan` holds flags to act on.
pub(crate) const F_PARAMS_RESCAN: u32 = 1 << 7;
/// Audio or note ports changed; the plugin must be reloaded to use them.
pub(crate) const F_PORTS_RESCAN: u32 = 1 << 8;

/// The plugin window asked for a new size (`gui_size`).
pub(crate) const F_GUI_RESIZE: u32 = 1 << 9;
/// The plugin asked the host to show its window.
pub(crate) const F_GUI_SHOW: u32 = 1 << 10;
/// The plugin asked the host to hide its window.
pub(crate) const F_GUI_HIDE: u32 = 1 << 11;
/// The plugin's window was closed or destroyed by the plugin.
pub(crate) const F_GUI_CLOSED: u32 = 1 << 12;

/// Flags `idle()` consumes. `F_RESTART` is left set on purpose. The
/// `F_GUI_*` flags are consumed by the plugin window code instead.
pub(crate) const MAIN_THREAD_FLAGS: u32 =
    F_CALLBACK | F_LATENCY | F_TAIL | F_STATE_DIRTY | F_PARAMS_RESCAN | F_PORTS_RESCAN;

/// State the callbacks write to. Lives inside [`HostBox`] so its address is
/// stable for the plugin's whole life.
pub(crate) struct HostShared {
    pub(crate) flags: AtomicU32,
    /// `CLAP_PARAM_RESCAN_*` bits accumulated since the last `idle()`.
    pub(crate) params_rescan: AtomicU32,
    /// Messages the audio thread tried to log. Logging allocates, so they are
    /// counted instead of printed.
    pub(crate) audio_thread_logs: AtomicU32,
    /// Prefix for log lines: the plugin's name.
    pub(crate) label: String,
    /// Size the plugin window asked for: width in the high 32 bits.
    pub(crate) gui_size: AtomicU64,
}

/// A `clap_host` together with the state its callbacks use.
pub(crate) struct HostBox {
    pub(crate) raw: clap_host,
    pub(crate) shared: HostShared,
}

// SAFETY: `raw` only holds pointers to immutable statics and to `shared`,
// which is atomics plus an immutable string.
unsafe impl Send for HostBox {}
unsafe impl Sync for HostBox {}

impl HostBox {
    pub(crate) fn new(label: &str) -> Box<HostBox> {
        let mut host = Box::new(HostBox {
            raw: clap_host {
                clap_version: CLAP_VERSION,
                host_data: std::ptr::null_mut(),
                name: c"Peachey Studio".as_ptr(),
                vendor: c"Peachey Studio".as_ptr(),
                url: c"https://github.com/hacchake/peachey-studio".as_ptr(),
                version: c"0.1.0".as_ptr(),
                get_extension: Some(get_extension),
                request_restart: Some(request_restart),
                request_process: Some(request_process),
                request_callback: Some(request_callback),
            },
            shared: HostShared {
                flags: AtomicU32::new(0),
                params_rescan: AtomicU32::new(0),
                audio_thread_logs: AtomicU32::new(0),
                label: label.to_string(),
                gui_size: AtomicU64::new(0),
            },
        });
        // The box never moves its contents, so this self-reference is stable.
        host.raw.host_data = (&host.shared as *const HostShared).cast_mut().cast();
        host
    }

    pub(crate) fn raw_ptr(&self) -> *const clap_host {
        &self.raw
    }
}

impl HostShared {
    pub(crate) fn raise(&self, flags: u32) {
        self.flags.fetch_or(flags, Ordering::AcqRel);
    }

    /// Clears `flags` and returns which of them were set.
    pub(crate) fn take(&self, flags: u32) -> u32 {
        self.flags.fetch_and(!flags, Ordering::AcqRel) & flags
    }

    pub(crate) fn is_raised(&self, flags: u32) -> bool {
        self.flags.load(Ordering::Acquire) & flags != 0
    }
}

/// Recovers the shared state from the `clap_host` a plugin passes back.
fn shared<'a>(host: *const clap_host) -> Option<&'a HostShared> {
    if host.is_null() {
        return None;
    }
    // SAFETY: the plugin got this pointer from us and `host_data` was set to
    // the `HostShared` in the same allocation, which outlives the plugin.
    let data = unsafe { (*host).host_data }.cast::<HostShared>();
    unsafe { data.as_ref() }
}

extern "C" fn request_restart(host: *const clap_host) {
    if let Some(s) = shared(host) {
        s.raise(F_RESTART);
    }
}

extern "C" fn request_process(host: *const clap_host) {
    if let Some(s) = shared(host) {
        s.raise(F_PROCESS);
    }
}

extern "C" fn request_callback(host: *const clap_host) {
    if let Some(s) = shared(host) {
        s.raise(F_CALLBACK);
    }
}

static THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(is_main_thread),
    is_audio_thread: Some(is_audio_thread),
};

extern "C" fn is_main_thread(_host: *const clap_host) -> bool {
    thread::is_main_thread()
}

extern "C" fn is_audio_thread(_host: *const clap_host) -> bool {
    thread::is_audio_thread()
}

static LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};

extern "C" fn host_log(host: *const clap_host, severity: clap_log_severity, msg: *const c_char) {
    let Some(s) = shared(host) else { return };
    if thread::current() == Role::Audio {
        // Formatting allocates and the logger may lock. Count it instead.
        s.audio_thread_logs.fetch_add(1, Ordering::Relaxed);
        return;
    }
    if msg.is_null() {
        return;
    }
    // SAFETY: the plugin passes a NUL-terminated string valid for the call.
    let text = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    let level = match severity {
        CLAP_LOG_DEBUG => log::Level::Debug,
        CLAP_LOG_INFO => log::Level::Info,
        CLAP_LOG_WARNING => log::Level::Warn,
        CLAP_LOG_ERROR
        | CLAP_LOG_FATAL
        | CLAP_LOG_HOST_MISBEHAVING
        | CLAP_LOG_PLUGIN_MISBEHAVING => log::Level::Error,
        _ => log::Level::Info,
    };
    log::log!(target: "peel_host_clap::plugin", level, "{}: {}", s.label, text);
}

static PARAMS: clap_host_params = clap_host_params {
    rescan: Some(params_rescan),
    clear: Some(params_clear),
    request_flush: Some(params_request_flush),
};

extern "C" fn params_rescan(host: *const clap_host, flags: u32) {
    if let Some(s) = shared(host) {
        s.params_rescan.fetch_or(flags, Ordering::AcqRel);
        s.raise(F_PARAMS_RESCAN);
    }
}

// The host stores no automation or modulation per parameter, so there is
// nothing to clear.
extern "C" fn params_clear(
    _host: *const clap_host,
    _param_id: u32,
    _flags: clap_param_clear_flags,
) {
}

extern "C" fn params_request_flush(host: *const clap_host) {
    if let Some(s) = shared(host) {
        s.raise(F_FLUSH);
    }
}

static LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(latency_changed),
};

extern "C" fn latency_changed(host: *const clap_host) {
    if let Some(s) = shared(host) {
        s.raise(F_LATENCY);
    }
}

static TAIL: clap_host_tail = clap_host_tail {
    changed: Some(tail_changed),
};

extern "C" fn tail_changed(host: *const clap_host) {
    if let Some(s) = shared(host) {
        s.raise(F_TAIL);
    }
}

static STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(state_mark_dirty),
};

extern "C" fn state_mark_dirty(host: *const clap_host) {
    if let Some(s) = shared(host) {
        s.raise(F_STATE_DIRTY);
    }
}

static AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(audio_ports_flag_supported),
    rescan: Some(audio_ports_rescan),
};

// Nothing is cached per port except what a reload rebuilds, so the host
// supports no in-place rescan; any request means "reload me".
extern "C" fn audio_ports_flag_supported(_host: *const clap_host, _flag: u32) -> bool {
    false
}

extern "C" fn audio_ports_rescan(host: *const clap_host, _flags: u32) {
    if let Some(s) = shared(host) {
        s.raise(F_PORTS_RESCAN);
    }
}

static NOTE_PORTS: clap_host_note_ports = clap_host_note_ports {
    supported_dialects: Some(note_dialects),
    rescan: Some(note_ports_rescan),
};

extern "C" fn note_dialects(_host: *const clap_host) -> clap_note_dialect {
    CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI
}

extern "C" fn note_ports_rescan(host: *const clap_host, _flags: u32) {
    if let Some(s) = shared(host) {
        s.raise(F_PORTS_RESCAN);
    }
}

static GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(gui_resize_hints_changed),
    request_resize: Some(gui_request_resize),
    request_show: Some(gui_request_show),
    request_hide: Some(gui_request_hide),
    closed: Some(gui_closed),
};

extern "C" fn gui_resize_hints_changed(_host: *const clap_host) {}

extern "C" fn gui_request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    // Thread-safe in CLAP: record it, the window code applies it on the main
    // thread.
    match shared(host) {
        Some(s) => {
            s.gui_size
                .store(((width as u64) << 32) | height as u64, Ordering::Release);
            s.raise(F_GUI_RESIZE);
            true
        }
        None => false,
    }
}

extern "C" fn gui_request_show(host: *const clap_host) -> bool {
    shared(host).map(|s| s.raise(F_GUI_SHOW)).is_some()
}

extern "C" fn gui_request_hide(host: *const clap_host) -> bool {
    shared(host).map(|s| s.raise(F_GUI_HIDE)).is_some()
}

extern "C" fn gui_closed(host: *const clap_host, _was_destroyed: bool) {
    if let Some(s) = shared(host) {
        s.raise(F_GUI_CLOSED);
    }
}

extern "C" fn get_extension(_host: *const clap_host, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return std::ptr::null();
    }
    // SAFETY: the plugin passes a NUL-terminated string valid for the call.
    let id = unsafe { CStr::from_ptr(id) };
    if id == CLAP_EXT_THREAD_CHECK {
        (&THREAD_CHECK as *const clap_host_thread_check).cast()
    } else if id == CLAP_EXT_LOG {
        (&LOG as *const clap_host_log).cast()
    } else if id == CLAP_EXT_PARAMS {
        (&PARAMS as *const clap_host_params).cast()
    } else if id == CLAP_EXT_LATENCY {
        (&LATENCY as *const clap_host_latency).cast()
    } else if id == CLAP_EXT_TAIL {
        (&TAIL as *const clap_host_tail).cast()
    } else if id == CLAP_EXT_STATE {
        (&STATE as *const clap_host_state).cast()
    } else if id == CLAP_EXT_AUDIO_PORTS {
        (&AUDIO_PORTS as *const clap_host_audio_ports).cast()
    } else if id == CLAP_EXT_NOTE_PORTS {
        (&NOTE_PORTS as *const clap_host_note_ports).cast()
    } else if id == CLAP_EXT_GUI {
        (&GUI as *const clap_host_gui).cast()
    } else {
        std::ptr::null()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread::RoleScope;

    #[test]
    fn requests_set_flags_from_any_thread() {
        let host = HostBox::new("test");
        let ptr = host.raw_ptr() as usize;
        std::thread::spawn(move || {
            let p = ptr as *const clap_host;
            request_callback(p);
            request_process(p);
            params_request_flush(p);
            latency_changed(p);
        })
        .join()
        .unwrap();
        let s = &host.shared;
        assert!(s.is_raised(F_CALLBACK | F_PROCESS | F_FLUSH | F_LATENCY));
        assert!(!s.is_raised(F_RESTART));
        assert_eq!(s.take(MAIN_THREAD_FLAGS), F_CALLBACK | F_LATENCY);
        assert!(s.is_raised(F_PROCESS | F_FLUSH));
    }

    #[test]
    fn rescan_flags_accumulate() {
        let host = HostBox::new("test");
        params_rescan(host.raw_ptr(), 1);
        params_rescan(host.raw_ptr(), 4);
        assert_eq!(host.shared.params_rescan.swap(0, Ordering::AcqRel), 5);
        assert!(host.shared.is_raised(F_PARAMS_RESCAN));
    }

    #[test]
    fn thread_check_reports_the_calling_role() {
        let host = HostBox::new("test");
        let p = host.raw_ptr();
        assert!(!is_main_thread(p) && !is_audio_thread(p));
        {
            let _main = RoleScope::main();
            assert!(is_main_thread(p) && !is_audio_thread(p));
        }
        let _audio = RoleScope::audio();
        assert!(!is_main_thread(p) && is_audio_thread(p));
    }

    #[test]
    fn audio_thread_logging_is_counted_not_printed() {
        let host = HostBox::new("test");
        let _audio = RoleScope::audio();
        host_log(host.raw_ptr(), CLAP_LOG_INFO, c"hello".as_ptr());
        assert_eq!(host.shared.audio_thread_logs.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn extensions_resolve_by_id() {
        let host = HostBox::new("test");
        let p = host.raw_ptr();
        for id in [
            CLAP_EXT_THREAD_CHECK,
            CLAP_EXT_LOG,
            CLAP_EXT_PARAMS,
            CLAP_EXT_LATENCY,
            CLAP_EXT_TAIL,
            CLAP_EXT_STATE,
            CLAP_EXT_AUDIO_PORTS,
            CLAP_EXT_NOTE_PORTS,
            CLAP_EXT_GUI,
        ] {
            assert!(!get_extension(p, id.as_ptr()).is_null(), "{id:?}");
        }
        assert!(get_extension(p, c"clap.nope".as_ptr()).is_null());
        assert!(get_extension(p, std::ptr::null()).is_null());
    }
}
