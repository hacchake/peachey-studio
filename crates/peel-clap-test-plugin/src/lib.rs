//! CLAP plugins used as test fixtures for `peel-host-clap`.
//!
//! One library, four plugins, written against `clap-sys` the way a real
//! plugin is (a `clap_entry` export, a factory, plain C callbacks):
//!
//! | Id | Role | Notes |
//! | --- | --- | --- |
//! | `dev.peel.test.gain` | stereo effect | `Gain` and `Invert` parameters, GUI and render extensions, state, sample-accurate parameter events |
//! | `dev.peel.test.mono-gain` | mono effect | same DSP on a one-channel port |
//! | `dev.peel.test.sine` | instrument | 8 voices, CLAP note events, a second (auxiliary) output port, a hidden and a read-only parameter, `NOTE_END` output events, reports latency 32 and a 100 ms tail |
//! | `dev.peel.test.midi-sine` | instrument | the same voices driven by MIDI 1.0 bytes only |
//!
//! The behavior is deliberately plain so tests can assert exact numbers:
//! gain multiplies, notes start and stop on the exact frame, and there are no
//! envelopes or smoothing.
//!
//! The plugins are strict hosts' guests: each one asks the host's
//! `clap.thread-check` extension whether it is being called on the right
//! thread (`process`, `reset` and processing start on the audio thread;
//! everything else on the main thread) and fails the call if not, so a
//! wrong thread role shows up as a test failure. A `Gain` value of exactly
//! 3.5 makes the effect behave like a plugin whose own GUI moved a knob: it
//! asks for a main-thread callback, and from there reports a latency change,
//! a parameter rescan, a state change and a restart request.
//!
//! The entry point also honours a few file names so the out-of-process
//! scanner can be tested: a library whose file name contains `crash` aborts
//! in `init`, `hang` never returns from it and `refuse` makes it fail.

#![allow(non_upper_case_globals)]
// Every callback is `unsafe extern "C"` and dereferences host pointers;
// wrapping each statement in an `unsafe` block would only add noise.
#![allow(unsafe_op_in_unsafe_fn)]

use std::ffi::{CStr, c_char, c_void};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use clap_sys::events::*;
use clap_sys::ext::audio_ports::*;
use clap_sys::ext::gui::*;
use clap_sys::ext::latency::*;
use clap_sys::ext::note_ports::*;
use clap_sys::ext::params::*;
use clap_sys::ext::render::*;
use clap_sys::ext::state::*;
use clap_sys::ext::tail::*;
use clap_sys::ext::thread_check::*;
use clap_sys::factory::plugin_factory::*;
use clap_sys::host::clap_host;
use clap_sys::id::CLAP_INVALID_ID;
use clap_sys::plugin::*;
use clap_sys::process::*;
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;

const GAIN_PARAM: u32 = 7;
const INVERT_PARAM: u32 = 9;
const LEVEL_PARAM: u32 = 100;
const SECRET_PARAM: u32 = 101;
const VOICES_PARAM: u32 = 102;

const GAIN_ID: &CStr = c"dev.peel.test.gain";
const MONO_ID: &CStr = c"dev.peel.test.mono-gain";
const SINE_ID: &CStr = c"dev.peel.test.sine";
const MIDI_SINE_ID: &CStr = c"dev.peel.test.midi-sine";

const STATE_MAGIC: &[u8; 4] = b"PTS1";
const VOICE_COUNT: usize = 8;

// ---- descriptors -------------------------------------------------------

struct Features<const N: usize>([*const c_char; N]);
// SAFETY: the pointers are to string literals.
unsafe impl<const N: usize> Sync for Features<N> {}

static GAIN_FEATURES: Features<4> = Features([
    c"audio-effect".as_ptr(),
    c"utility".as_ptr(),
    c"stereo".as_ptr(),
    null(),
]);
static MONO_FEATURES: Features<4> = Features([
    c"audio-effect".as_ptr(),
    c"utility".as_ptr(),
    c"mono".as_ptr(),
    null(),
]);
static SINE_FEATURES: Features<4> = Features([
    c"instrument".as_ptr(),
    c"synthesizer".as_ptr(),
    c"stereo".as_ptr(),
    null(),
]);

macro_rules! descriptor {
    ($id:expr, $name:expr, $text:expr, $features:expr) => {
        clap_plugin_descriptor {
            clap_version: CLAP_VERSION,
            id: $id.as_ptr(),
            name: $name.as_ptr(),
            vendor: c"Peel Studio".as_ptr(),
            url: c"https://github.com/Vheissu/peel-studio".as_ptr(),
            manual_url: c"".as_ptr(),
            support_url: c"".as_ptr(),
            version: c"1.2.3".as_ptr(),
            description: $text.as_ptr(),
            features: $features.0.as_ptr(),
        }
    };
}

static GAIN_DESC: clap_plugin_descriptor = descriptor!(
    GAIN_ID,
    c"Test Gain",
    c"Multiplies the signal",
    GAIN_FEATURES
);
static MONO_DESC: clap_plugin_descriptor = descriptor!(
    MONO_ID,
    c"Test Mono Gain",
    c"Multiplies a mono signal",
    MONO_FEATURES
);
static SINE_DESC: clap_plugin_descriptor =
    descriptor!(SINE_ID, c"Test Sine", c"Plays sine tones", SINE_FEATURES);
static MIDI_SINE_DESC: clap_plugin_descriptor = descriptor!(
    MIDI_SINE_ID,
    c"Test MIDI Sine",
    c"Plays sine tones from MIDI",
    SINE_FEATURES
);

fn descriptors() -> [&'static clap_plugin_descriptor; 4] {
    [&GAIN_DESC, &MONO_DESC, &SINE_DESC, &MIDI_SINE_DESC]
}

// ---- plugin instance ---------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Gain { channels: u32 },
    Sine { midi: bool },
}

#[derive(Clone, Copy)]
struct Voice {
    active: bool,
    note_id: i32,
    key: i16,
    phase: f64,
    step: f64,
    amp: f32,
}

const IDLE_VOICE: Voice = Voice {
    active: false,
    note_id: -1,
    key: 0,
    phase: 0.0,
    step: 0.0,
    amp: 0.0,
};

const REQUEST_ALL: u8 = 1;

struct Plugin {
    clap: clap_plugin,
    host: *const clap_host,
    thread_check: *const clap_host_thread_check,
    kind: Kind,
    sample_rate: f64,
    gain: f32,
    invert: bool,
    level: f32,
    offline: bool,
    latency: u32,
    /// Work for `on_main_thread`, set from the audio thread.
    requests: AtomicU8,
    /// A call arrived on the wrong thread.
    bad_thread: bool,
    voices: [Voice; VOICE_COUNT],
    /// The gain plugin's window (a Win32 child of the host's window).
    gui_child: *mut c_void,
}

unsafe fn plugin_of<'a>(plugin: *const clap_plugin) -> &'a mut Plugin {
    &mut *((*plugin).plugin_data as *mut Plugin)
}

impl Plugin {
    fn gain_factor(&self) -> f32 {
        if self.invert { -self.gain } else { self.gain }
    }

    /// True when the host says this thread has the role the call needs.
    unsafe fn on_thread(&self, audio: bool) -> bool {
        let Some(check) = self.thread_check.as_ref() else {
            return true;
        };
        let (Some(is_main), Some(is_audio)) = (check.is_main_thread, check.is_audio_thread) else {
            return true;
        };
        let (main, aud) = (is_main(self.host), is_audio(self.host));
        if audio { aud && !main } else { main && !aud }
    }

    unsafe fn set_param(&mut self, id: u32, value: f64) {
        match (self.kind, id) {
            (Kind::Gain { .. }, GAIN_PARAM) if value == 3.5 => {
                self.requests.store(REQUEST_ALL, Ordering::SeqCst);
                if let Some(request) = self.host.as_ref().and_then(|h| h.request_callback) {
                    request(self.host);
                }
            }
            (Kind::Gain { .. }, GAIN_PARAM) => self.gain = value as f32,
            (Kind::Gain { .. }, INVERT_PARAM) => self.invert = value >= 0.5,
            (Kind::Sine { .. }, LEVEL_PARAM) => self.level = value as f32,
            _ => {}
        }
    }

    fn note_on(&mut self, note_id: i32, key: i16, velocity: f64) {
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or(0);
        let freq = 440.0 * 2f64.powf((f64::from(key) - 69.0) / 12.0);
        self.voices[slot] = Voice {
            active: true,
            note_id,
            key,
            phase: 0.0,
            step: std::f64::consts::TAU * freq / self.sample_rate,
            amp: velocity as f32 * self.level,
        };
    }

    /// Releases matching voices; `-1` is a wildcard. Returns the ended ones.
    fn note_off(&mut self, note_id: i32, key: i16, out: &mut [Option<Voice>; VOICE_COUNT]) {
        for (slot, voice) in out.iter_mut().zip(self.voices.iter_mut()) {
            let id_matches = note_id == -1 || voice.note_id == note_id;
            let key_matches = key == -1 || voice.key == key;
            if voice.active && id_matches && key_matches {
                voice.active = false;
                *slot = Some(*voice);
            }
        }
    }

    unsafe fn handle_event(&mut self, header: &clap_event_header, out: *const clap_output_events) {
        if header.space_id != CLAP_CORE_EVENT_SPACE_ID {
            return;
        }
        let mut ended: [Option<Voice>; VOICE_COUNT] = [None; VOICE_COUNT];
        match header.type_ {
            CLAP_EVENT_PARAM_VALUE => {
                let e = &*(header as *const _ as *const clap_event_param_value);
                self.set_param(e.param_id, e.value);
            }
            CLAP_EVENT_NOTE_ON if self.kind == (Kind::Sine { midi: false }) => {
                let e = &*(header as *const _ as *const clap_event_note);
                self.note_on(e.note_id, e.key, e.velocity);
            }
            CLAP_EVENT_NOTE_OFF | CLAP_EVENT_NOTE_CHOKE
                if self.kind == (Kind::Sine { midi: false }) =>
            {
                let e = &*(header as *const _ as *const clap_event_note);
                self.note_off(e.note_id, e.key, &mut ended);
            }
            CLAP_EVENT_MIDI if self.kind == (Kind::Sine { midi: true }) => {
                let e = &*(header as *const _ as *const clap_event_midi);
                let [status, a, b] = e.data;
                match (status & 0xF0, b) {
                    (0x90, v) if v > 0 => self.note_on(-1, i16::from(a), f64::from(v) / 127.0),
                    (0x90, _) | (0x80, _) => self.note_off(-1, i16::from(a), &mut ended),
                    (0xB0, _) if a == 123 || a == 120 => self.note_off(-1, -1, &mut ended),
                    _ => {}
                }
            }
            _ => {}
        }
        if let Some(try_push) = out.as_ref().and_then(|o| o.try_push) {
            for voice in ended.iter().flatten() {
                let end = clap_event_note {
                    header: clap_event_header {
                        size: size_of::<clap_event_note>() as u32,
                        time: header.time,
                        space_id: CLAP_CORE_EVENT_SPACE_ID,
                        type_: CLAP_EVENT_NOTE_END,
                        flags: 0,
                    },
                    note_id: voice.note_id,
                    port_index: 0,
                    channel: 0,
                    key: voice.key,
                    velocity: 0.0,
                };
                try_push(out, &end.header);
            }
        }
    }
}

unsafe extern "C" fn plugin_init(plugin: *const clap_plugin) -> bool {
    let p = plugin_of(plugin);
    if let Some(get) = p.host.as_ref().and_then(|h| h.get_extension) {
        p.thread_check = get(p.host, CLAP_EXT_THREAD_CHECK.as_ptr()).cast();
    }
    p.on_thread(false)
}

unsafe extern "C" fn plugin_destroy(plugin: *const clap_plugin) {
    drop(Box::from_raw((*plugin).plugin_data as *mut Plugin));
}

unsafe extern "C" fn plugin_activate(
    plugin: *const clap_plugin,
    sample_rate: f64,
    _min: u32,
    _max: u32,
) -> bool {
    let p = plugin_of(plugin);
    p.sample_rate = sample_rate;
    p.on_thread(false)
}

unsafe extern "C" fn plugin_deactivate(plugin: *const clap_plugin) {
    let p = plugin_of(plugin);
    p.bad_thread |= !p.on_thread(false);
}

unsafe extern "C" fn plugin_start(plugin: *const clap_plugin) -> bool {
    plugin_of(plugin).on_thread(true)
}

unsafe extern "C" fn plugin_stop(plugin: *const clap_plugin) {
    let p = plugin_of(plugin);
    p.bad_thread |= !p.on_thread(true);
}

unsafe extern "C" fn plugin_reset(plugin: *const clap_plugin) {
    let p = plugin_of(plugin);
    p.bad_thread |= !p.on_thread(true);
    p.voices = [IDLE_VOICE; VOICE_COUNT];
}

/// Plays the part of a plugin whose own GUI changed things: it reports a
/// latency change, new parameter values, a dirty state and a restart.
unsafe extern "C" fn plugin_on_main_thread(plugin: *const clap_plugin) {
    let p = plugin_of(plugin);
    if !p.on_thread(false) {
        p.bad_thread = true;
        return;
    }
    if p.requests.swap(0, Ordering::SeqCst) == 0 {
        return;
    }
    let Some(host) = p.host.as_ref() else { return };
    let Some(get) = host.get_extension else {
        return;
    };
    p.latency = 64;
    p.gain = 0.75;
    let latency = get(p.host, CLAP_EXT_LATENCY.as_ptr()).cast::<clap_host_latency>();
    if let Some(changed) = latency.as_ref().and_then(|l| l.changed) {
        changed(p.host);
    }
    let params = get(p.host, CLAP_EXT_PARAMS.as_ptr()).cast::<clap_host_params>();
    if let Some(rescan) = params.as_ref().and_then(|x| x.rescan) {
        rescan(p.host, CLAP_PARAM_RESCAN_VALUES);
    }
    let state = get(p.host, CLAP_EXT_STATE.as_ptr()).cast::<clap_host_state>();
    if let Some(dirty) = state.as_ref().and_then(|x| x.mark_dirty) {
        dirty(p.host);
    }
    if let Some(restart) = host.request_restart {
        restart(p.host);
    }
}

unsafe extern "C" fn plugin_process(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> clap_process_status {
    let p = plugin_of(plugin);
    if !p.on_thread(true) {
        return CLAP_PROCESS_ERROR;
    }
    let pr = &*process;
    let events = &*pr.in_events;
    let (Some(size), Some(get)) = (events.size, events.get) else {
        return CLAP_PROCESS_ERROR;
    };
    let count = size(events);
    let frames = pr.frames_count as usize;
    let mut next = 0u32;

    match p.kind {
        Kind::Gain { .. } => {
            let input = &*pr.audio_inputs;
            let output = &*pr.audio_outputs;
            let channels = input.channel_count.min(output.channel_count) as usize;
            let mut pos = 0usize;
            while pos < frames {
                while next < count {
                    let header = &*get(events, next);
                    if header.time as usize > pos {
                        break;
                    }
                    p.handle_event(header, pr.out_events);
                    next += 1;
                }
                let end = if next < count {
                    ((*get(events, next)).time as usize).min(frames)
                } else {
                    frames
                };
                let factor = p.gain_factor();
                for c in 0..channels {
                    let src = *input.data32.add(c);
                    let dst = *output.data32.add(c);
                    for i in pos..end {
                        *dst.add(i) = *src.add(i) * factor;
                    }
                }
                pos = end;
            }
            CLAP_PROCESS_CONTINUE
        }
        Kind::Sine { .. } => {
            let main = &*pr.audio_outputs;
            let left = *main.data32;
            let right = *main.data32.add(1);
            // The second, auxiliary output gets a constant so tests can prove
            // the host ignores it.
            let aux = (pr.audio_outputs_count > 1).then(|| *(*pr.audio_outputs.add(1)).data32);
            for i in 0..frames {
                while next < count {
                    let header = &*get(events, next);
                    if header.time as usize > i {
                        break;
                    }
                    p.handle_event(header, pr.out_events);
                    next += 1;
                }
                let mut sample = 0.0f32;
                for voice in p.voices.iter_mut().filter(|v| v.active) {
                    sample += voice.phase.sin() as f32 * voice.amp;
                    voice.phase += voice.step;
                }
                *left.add(i) = sample;
                *right.add(i) = sample;
                if let Some(aux) = aux {
                    *aux.add(i) = 0.9;
                }
            }
            if p.voices.iter().any(|v| v.active) {
                CLAP_PROCESS_CONTINUE
            } else {
                CLAP_PROCESS_SLEEP
            }
        }
    }
}

// ---- extensions ----------------------------------------------------------

unsafe extern "C" fn audio_ports_count(plugin: *const clap_plugin, is_input: bool) -> u32 {
    match (plugin_of(plugin).kind, is_input) {
        (Kind::Gain { .. }, _) => 1,
        (Kind::Sine { .. }, true) => 0,
        (Kind::Sine { .. }, false) => 2,
    }
}

unsafe extern "C" fn audio_ports_get(
    plugin: *const clap_plugin,
    index: u32,
    _is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    let info = &mut *info;
    info.id = index;
    info.in_place_pair = CLAP_INVALID_ID;
    match (plugin_of(plugin).kind, index) {
        (Kind::Gain { channels }, 0) => {
            copy_str(&mut info.name, "main");
            info.flags = CLAP_AUDIO_PORT_IS_MAIN;
            info.channel_count = channels;
            info.port_type = if channels == 1 {
                CLAP_PORT_MONO.as_ptr()
            } else {
                CLAP_PORT_STEREO.as_ptr()
            };
            true
        }
        (Kind::Sine { .. }, 0) => {
            copy_str(&mut info.name, "main");
            info.flags = CLAP_AUDIO_PORT_IS_MAIN;
            info.channel_count = 2;
            info.port_type = CLAP_PORT_STEREO.as_ptr();
            true
        }
        (Kind::Sine { .. }, 1) => {
            copy_str(&mut info.name, "aux");
            info.flags = 0;
            info.channel_count = 1;
            info.port_type = CLAP_PORT_MONO.as_ptr();
            true
        }
        _ => false,
    }
}

static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
    count: Some(audio_ports_count),
    get: Some(audio_ports_get),
};

unsafe extern "C" fn note_ports_count(plugin: *const clap_plugin, is_input: bool) -> u32 {
    u32::from(is_input && matches!(plugin_of(plugin).kind, Kind::Sine { .. }))
}

unsafe extern "C" fn note_ports_get(
    plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    let Kind::Sine { midi } = plugin_of(plugin).kind else {
        return false;
    };
    if index != 0 || !is_input {
        return false;
    }
    let info = &mut *info;
    let dialect = if midi {
        CLAP_NOTE_DIALECT_MIDI
    } else {
        CLAP_NOTE_DIALECT_CLAP
    };
    info.id = 0;
    info.supported_dialects = dialect;
    info.preferred_dialect = dialect;
    copy_str(&mut info.name, "notes");
    true
}

static NOTE_PORTS: clap_plugin_note_ports = clap_plugin_note_ports {
    count: Some(note_ports_count),
    get: Some(note_ports_get),
};

fn param_ids(kind: Kind) -> &'static [u32] {
    match kind {
        Kind::Gain { .. } => &[GAIN_PARAM, INVERT_PARAM],
        Kind::Sine { .. } => &[LEVEL_PARAM, SECRET_PARAM, VOICES_PARAM],
    }
}

unsafe extern "C" fn params_count(plugin: *const clap_plugin) -> u32 {
    let p = plugin_of(plugin);
    if !p.on_thread(false) {
        return 0;
    }
    param_ids(p.kind).len() as u32
}

unsafe extern "C" fn params_get_info(
    plugin: *const clap_plugin,
    index: u32,
    info: *mut clap_param_info,
) -> bool {
    let Some(&id) = param_ids(plugin_of(plugin).kind).get(index as usize) else {
        return false;
    };
    let info = &mut *info;
    info.id = id;
    info.cookie = (id as usize + 0x1000) as *mut c_void;
    match id {
        GAIN_PARAM => {
            info.flags = CLAP_PARAM_IS_AUTOMATABLE;
            copy_str(&mut info.name, "Gain");
            copy_str(&mut info.module, "Main");
            (info.min_value, info.max_value, info.default_value) = (0.0, 4.0, 1.0);
        }
        INVERT_PARAM => {
            info.flags = CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_ENUM;
            copy_str(&mut info.name, "Phase");
            copy_str(&mut info.module, "Main/Phase");
            (info.min_value, info.max_value, info.default_value) = (0.0, 1.0, 0.0);
        }
        LEVEL_PARAM => {
            info.flags = CLAP_PARAM_IS_AUTOMATABLE;
            copy_str(&mut info.name, "Level");
            copy_str(&mut info.module, "Osc");
            (info.min_value, info.max_value, info.default_value) = (0.0, 1.0, 0.5);
        }
        SECRET_PARAM => {
            info.flags = CLAP_PARAM_IS_HIDDEN;
            copy_str(&mut info.name, "Secret");
            (info.min_value, info.max_value, info.default_value) = (0.0, 1.0, 0.0);
        }
        _ => {
            info.flags = CLAP_PARAM_IS_READONLY;
            copy_str(&mut info.name, "Voices");
            (info.min_value, info.max_value, info.default_value) = (0.0, 8.0, 0.0);
        }
    }
    true
}

unsafe extern "C" fn params_get_value(plugin: *const clap_plugin, id: u32, out: *mut f64) -> bool {
    let p = plugin_of(plugin);
    if !p.on_thread(false) {
        return false;
    }
    *out = match id {
        GAIN_PARAM => f64::from(p.gain),
        INVERT_PARAM => f64::from(u8::from(p.invert)),
        LEVEL_PARAM => f64::from(p.level),
        SECRET_PARAM => 0.0,
        VOICES_PARAM => p.voices.iter().filter(|v| v.active).count() as f64,
        _ => return false,
    };
    true
}

unsafe extern "C" fn params_value_to_text(
    plugin: *const clap_plugin,
    id: u32,
    value: f64,
    out: *mut c_char,
    capacity: u32,
) -> bool {
    if !plugin_of(plugin).on_thread(false) {
        return false;
    }
    let text = match id {
        GAIN_PARAM => format!("{value:.2}x"),
        INVERT_PARAM => if value >= 0.5 { "Inverted" } else { "Normal" }.to_string(),
        _ => format!("{value:.2}"),
    };
    let dst = std::slice::from_raw_parts_mut(out, capacity as usize);
    copy_str(dst, &text);
    true
}

unsafe extern "C" fn params_text_to_value(
    _plugin: *const clap_plugin,
    _id: u32,
    text: *const c_char,
    out: *mut f64,
) -> bool {
    let text = CStr::from_ptr(text).to_string_lossy();
    match text.trim().trim_end_matches('x').parse::<f64>() {
        Ok(v) => {
            *out = v;
            true
        }
        Err(_) => false,
    }
}

unsafe extern "C" fn params_flush(
    plugin: *const clap_plugin,
    input: *const clap_input_events,
    output: *const clap_output_events,
) {
    let p = plugin_of(plugin);
    let events = &*input;
    let (Some(size), Some(get)) = (events.size, events.get) else {
        return;
    };
    for i in 0..size(events) {
        p.handle_event(&*get(events, i), output);
    }
}

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: Some(params_text_to_value),
    flush: Some(params_flush),
};

unsafe extern "C" fn state_save(plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
    let p = plugin_of(plugin);
    // Fails if any earlier call came from the wrong thread, so a test sees
    // problems in calls that return nothing.
    if p.bad_thread || !p.on_thread(false) {
        return false;
    }
    let mut bytes = STATE_MAGIC.to_vec();
    match p.kind {
        Kind::Gain { .. } => {
            bytes.extend_from_slice(&p.gain.to_le_bytes());
            bytes.push(u8::from(p.invert));
        }
        Kind::Sine { .. } => bytes.extend_from_slice(&p.level.to_le_bytes()),
    }
    let Some(write) = (*stream).write else {
        return false;
    };
    let mut done = 0usize;
    while done < bytes.len() {
        let n = write(
            stream,
            bytes.as_ptr().add(done).cast(),
            (bytes.len() - done) as u64,
        );
        if n <= 0 {
            return false;
        }
        done += n as usize;
    }
    true
}

unsafe extern "C" fn state_load(plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
    let p = plugin_of(plugin);
    if !p.on_thread(false) {
        return false;
    }
    let want = match p.kind {
        Kind::Gain { .. } => 4 + 4 + 1,
        Kind::Sine { .. } => 4 + 4,
    };
    let Some(read) = (*stream).read else {
        return false;
    };
    let mut bytes = vec![0u8; want];
    let mut done = 0usize;
    while done < want {
        let n = read(
            stream,
            bytes.as_mut_ptr().add(done).cast(),
            (want - done) as u64,
        );
        if n <= 0 {
            return false;
        }
        done += n as usize;
    }
    if &bytes[..4] != STATE_MAGIC {
        return false;
    }
    let value = f32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    match p.kind {
        Kind::Gain { .. } => {
            p.gain = value;
            p.invert = bytes[8] != 0;
        }
        Kind::Sine { .. } => p.level = value,
    }
    true
}

static STATE: clap_plugin_state = clap_plugin_state {
    save: Some(state_save),
    load: Some(state_load),
};

unsafe extern "C" fn latency_get(plugin: *const clap_plugin) -> u32 {
    // Declared only: the fixture does not delay its output.
    let p = plugin_of(plugin);
    match p.kind {
        Kind::Sine { .. } => 32,
        Kind::Gain { .. } => p.latency,
    }
}

static LATENCY: clap_plugin_latency = clap_plugin_latency {
    get: Some(latency_get),
};

unsafe extern "C" fn tail_get(plugin: *const clap_plugin) -> u32 {
    match plugin_of(plugin).kind {
        Kind::Sine { .. } => 4800,
        Kind::Gain { .. } => 0,
    }
}

static TAIL: clap_plugin_tail = clap_plugin_tail {
    get: Some(tail_get),
};

// The gain plugin's window: on Windows, a static text control embedded in
// the host's window. It asks the host to grow once it is attached, so hosts
// can test `request_resize`. Elsewhere no window can be created.
const GUI_SIZE: (u32, u32) = (360, 200);
/// What the window asks to grow to after `set_parent`.
pub const GUI_REQUESTED_SIZE: (u32, u32) = (400, 240);

unsafe extern "C" fn gui_is_api_supported(
    _plugin: *const clap_plugin,
    api: *const c_char,
    floating: bool,
) -> bool {
    cfg!(windows) && !floating && !api.is_null() && CStr::from_ptr(api) == CLAP_WINDOW_API_WIN32
}

unsafe extern "C" fn gui_create(
    plugin: *const clap_plugin,
    api: *const c_char,
    floating: bool,
) -> bool {
    gui_is_api_supported(plugin, api, floating)
}

unsafe extern "C" fn gui_destroy(plugin: *const clap_plugin) {
    let p = plugin_of(plugin);
    if !p.gui_child.is_null() {
        #[cfg(windows)]
        windows_sys::Win32::UI::WindowsAndMessaging::DestroyWindow(p.gui_child);
        p.gui_child = null_mut();
    }
}

unsafe extern "C" fn gui_set_scale(_plugin: *const clap_plugin, _scale: f64) -> bool {
    true
}

unsafe extern "C" fn gui_get_size(plugin: *const clap_plugin, w: *mut u32, h: *mut u32) -> bool {
    let _ = plugin;
    *w = GUI_SIZE.0;
    *h = GUI_SIZE.1;
    true
}

unsafe extern "C" fn gui_can_resize(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn gui_adjust_size(
    _plugin: *const clap_plugin,
    _w: *mut u32,
    _h: *mut u32,
) -> bool {
    true
}

unsafe extern "C" fn gui_set_size(plugin: *const clap_plugin, w: u32, h: u32) -> bool {
    let p = plugin_of(plugin);
    #[cfg(windows)]
    if !p.gui_child.is_null() {
        windows_sys::Win32::UI::WindowsAndMessaging::MoveWindow(
            p.gui_child,
            0,
            0,
            w as i32,
            h as i32,
            1,
        );
    }
    let _ = (p, w, h);
    true
}

unsafe extern "C" fn gui_set_parent(
    plugin: *const clap_plugin,
    window: *const clap_window,
) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::*;
        let p = plugin_of(plugin);
        let text: Vec<u16> = "Peel test gain".encode_utf16().chain([0]).collect();
        let class: Vec<u16> = "STATIC".encode_utf16().chain([0]).collect();
        p.gui_child = CreateWindowExW(
            0,
            class.as_ptr(),
            text.as_ptr(),
            WS_CHILD | WS_VISIBLE | 1, // SS_CENTER
            0,
            0,
            GUI_SIZE.0 as i32,
            GUI_SIZE.1 as i32,
            (*window).specific.win32,
            null_mut(),
            null_mut(),
            null(),
        );
        if p.gui_child.is_null() {
            return false;
        }
        let get = (*p.host).get_extension.expect("get_extension");
        let gui = get(p.host, CLAP_EXT_GUI.as_ptr()).cast::<clap_host_gui>();
        if let Some(resize) = gui.as_ref().and_then(|g| g.request_resize) {
            resize(p.host, GUI_REQUESTED_SIZE.0, GUI_REQUESTED_SIZE.1);
        }
        true
    }
    #[cfg(not(windows))]
    {
        let _ = (plugin, window);
        false
    }
}

unsafe extern "C" fn gui_show(plugin: *const clap_plugin) -> bool {
    #[cfg(windows)]
    windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(plugin_of(plugin).gui_child, 5);
    let _ = plugin;
    true
}

unsafe extern "C" fn gui_hide(plugin: *const clap_plugin) -> bool {
    #[cfg(windows)]
    windows_sys::Win32::UI::WindowsAndMessaging::ShowWindow(plugin_of(plugin).gui_child, 0);
    let _ = plugin;
    true
}

static GUI: clap_plugin_gui = clap_plugin_gui {
    is_api_supported: Some(gui_is_api_supported),
    get_preferred_api: None,
    create: Some(gui_create),
    destroy: Some(gui_destroy),
    set_scale: Some(gui_set_scale),
    get_size: Some(gui_get_size),
    can_resize: Some(gui_can_resize),
    get_resize_hints: None,
    adjust_size: Some(gui_adjust_size),
    set_size: Some(gui_set_size),
    set_parent: Some(gui_set_parent),
    set_transient: None,
    suggest_title: None,
    show: Some(gui_show),
    hide: Some(gui_hide),
};

unsafe extern "C" fn render_realtime_only(_plugin: *const clap_plugin) -> bool {
    false
}

unsafe extern "C" fn render_set(plugin: *const clap_plugin, mode: clap_plugin_render_mode) -> bool {
    plugin_of(plugin).offline = mode == CLAP_RENDER_OFFLINE;
    true
}

static RENDER: clap_plugin_render = clap_plugin_render {
    has_hard_realtime_requirement: Some(render_realtime_only),
    set: Some(render_set),
};

unsafe extern "C" fn plugin_get_extension(
    plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    let kind = plugin_of(plugin).kind;
    let id = CStr::from_ptr(id);
    let is_gain = matches!(kind, Kind::Gain { .. });
    let is_sine = matches!(kind, Kind::Sine { .. });
    let midi_sine = kind == (Kind::Sine { midi: true });
    if id == CLAP_EXT_AUDIO_PORTS {
        (&AUDIO_PORTS as *const clap_plugin_audio_ports).cast()
    } else if id == CLAP_EXT_NOTE_PORTS && is_sine {
        (&NOTE_PORTS as *const clap_plugin_note_ports).cast()
    } else if id == CLAP_EXT_PARAMS {
        (&PARAMS as *const clap_plugin_params).cast()
    } else if id == CLAP_EXT_STATE {
        (&STATE as *const clap_plugin_state).cast()
    } else if id == CLAP_EXT_LATENCY {
        (&LATENCY as *const clap_plugin_latency).cast()
    } else if id == CLAP_EXT_TAIL && !midi_sine {
        // The MIDI instrument has no tail extension, to test "unknown".
        (&TAIL as *const clap_plugin_tail).cast()
    } else if id == CLAP_EXT_GUI && is_gain {
        (&GUI as *const clap_plugin_gui).cast()
    } else if id == CLAP_EXT_RENDER && is_gain {
        (&RENDER as *const clap_plugin_render).cast()
    } else {
        null()
    }
}

/// Writes `text` into a fixed C buffer, truncating, always NUL-terminated.
fn copy_str(dst: &mut [c_char], text: &str) {
    if dst.is_empty() {
        return;
    }
    let n = text.len().min(dst.len() - 1);
    for (d, s) in dst.iter_mut().zip(text.bytes()).take(n) {
        *d = s as c_char;
    }
    dst[n] = 0;
}

// ---- factory and entry ----------------------------------------------------

unsafe extern "C" fn factory_count(_factory: *const clap_plugin_factory) -> u32 {
    descriptors().len() as u32
}

unsafe extern "C" fn factory_descriptor(
    _factory: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    descriptors()
        .get(index as usize)
        .map_or(null(), |d| *d as *const _)
}

unsafe extern "C" fn factory_create(
    _factory: *const clap_plugin_factory,
    host: *const clap_host,
    id: *const c_char,
) -> *const clap_plugin {
    if id.is_null() {
        return null();
    }
    let id = CStr::from_ptr(id);
    let (desc, kind) = if id == GAIN_ID {
        (&GAIN_DESC, Kind::Gain { channels: 2 })
    } else if id == MONO_ID {
        (&MONO_DESC, Kind::Gain { channels: 1 })
    } else if id == SINE_ID {
        (&SINE_DESC, Kind::Sine { midi: false })
    } else if id == MIDI_SINE_ID {
        (&MIDI_SINE_DESC, Kind::Sine { midi: true })
    } else {
        return null();
    };
    let plugin = Box::into_raw(Box::new(Plugin {
        clap: clap_plugin {
            desc,
            plugin_data: null_mut(),
            init: Some(plugin_init),
            destroy: Some(plugin_destroy),
            activate: Some(plugin_activate),
            deactivate: Some(plugin_deactivate),
            start_processing: Some(plugin_start),
            stop_processing: Some(plugin_stop),
            reset: Some(plugin_reset),
            process: Some(plugin_process),
            get_extension: Some(plugin_get_extension),
            on_main_thread: Some(plugin_on_main_thread),
        },
        host,
        thread_check: null(),
        kind,
        sample_rate: 44_100.0,
        gain: 1.0,
        invert: false,
        level: 0.5,
        offline: false,
        latency: 0,
        requests: AtomicU8::new(0),
        bad_thread: false,
        voices: [IDLE_VOICE; VOICE_COUNT],
        gui_child: null_mut(),
    }));
    (*plugin).clap.plugin_data = plugin.cast();
    &(*plugin).clap
}

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_count),
    get_plugin_descriptor: Some(factory_descriptor),
    create_plugin: Some(factory_create),
};

unsafe extern "C" fn entry_init(path: *const c_char) -> bool {
    if !path.is_null() {
        let path = CStr::from_ptr(path).to_string_lossy();
        let name = path.rsplit(['/', '\\']).next().unwrap_or("");
        if name.contains("crash") {
            std::process::abort();
        }
        if name.contains("hang") {
            loop {
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        if name.contains("refuse") {
            return false;
        }
    }
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn entry_get_factory(id: *const c_char) -> *const c_void {
    if !id.is_null() && CStr::from_ptr(id) == CLAP_PLUGIN_FACTORY_ID {
        (&FACTORY as *const clap_plugin_factory).cast()
    } else {
        null()
    }
}

#[unsafe(no_mangle)]
pub static clap_entry: clap_sys::entry::clap_plugin_entry = clap_sys::entry::clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_get_factory),
};
