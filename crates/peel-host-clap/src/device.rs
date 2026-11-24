//! [`ClapDevice`]: a CLAP plugin as a `peel_dsp::Device`.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_NOTE_CHOKE, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
    CLAP_EVENT_TRANSPORT, CLAP_NOTE_EXPRESSION_PRESSURE, CLAP_NOTE_EXPRESSION_TUNING,
    CLAP_TRANSPORT_HAS_BEATS_TIMELINE, CLAP_TRANSPORT_HAS_TEMPO, CLAP_TRANSPORT_IS_PLAYING,
    clap_event_header, clap_event_transport,
};
use clap_sys::ext::note_ports::{
    CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, CLAP_NOTE_DIALECT_MIDI_MPE,
};
use clap_sys::fixedpoint::CLAP_BEATTIME_FACTOR;
use clap_sys::process::{
    CLAP_PROCESS_CONTINUE_IF_NOT_QUIET, CLAP_PROCESS_ERROR, CLAP_PROCESS_SLEEP, clap_process,
};
use peel_dsp::{
    Device, DeviceDescriptor, DeviceEvent, DeviceKind, EventKind, ParamId, ProcessContext,
};

use crate::audio::AudioBuffers;
use crate::descriptor::{self, DescriptorSpec};
use crate::error::ClapError;
use crate::events::{self, EventList};
use crate::handle::{ClapHandle, ClapStats};
use crate::host::{F_FLUSH, F_PROCESS, F_RESTART};
use crate::info::{ClapPluginInfo, ClapRole};
use crate::instance::{IdleReport, NotePortInfo, PluginShared};
use crate::module::ModuleHandle;
use crate::params::{ParamEntry, PendingParams};
use crate::thread::RoleScope;

/// Event slots per device beyond one slot per parameter.
const BASE_EVENT_SLOTS: usize = 2048;
/// Slots only releases and panics may use: enough for a release of every
/// tracked note plus the wildcard.
const RELEASE_RESERVE: usize = MAX_TRACKED_NOTES + 128;
/// Notes remembered for `AllNotesOff`. More than this still play; only the
/// explicit per-note release on a panic is lost, and the wildcard release
/// still covers them.
pub(super) const MAX_TRACKED_NOTES: usize = 512;
/// Output below this after a `CLAP_PROCESS_CONTINUE_IF_NOT_QUIET` is quiet
/// (about -180 dBFS).
const QUIET_PEAK: f32 = 1e-9;

/// How notes reach the plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Dialect {
    /// CLAP note events.
    Clap,
    /// MIDI 1.0 bytes, for plugins that accept nothing else.
    Midi,
    /// The plugin has no usable note input; notes are dropped.
    None,
}

fn pick_dialect(ports: &[NotePortInfo]) -> Dialect {
    let Some(port) = ports.first() else {
        return Dialect::None;
    };
    if port.supported_dialects & CLAP_NOTE_DIALECT_CLAP != 0 {
        Dialect::Clap
    } else if port.supported_dialects & (CLAP_NOTE_DIALECT_MIDI | CLAP_NOTE_DIALECT_MIDI_MPE) != 0 {
        Dialect::Midi
    } else {
        Dialect::None
    }
}

/// Notes the plugin is holding, for `AllNotesOff`.
struct ActiveNotes {
    items: Vec<(i32, i16)>,
}

impl ActiveNotes {
    fn new() -> ActiveNotes {
        ActiveNotes {
            items: Vec::with_capacity(MAX_TRACKED_NOTES),
        }
    }

    fn add(&mut self, id: i32, key: i16) {
        if self.items.len() < self.items.capacity() {
            self.items.push((id, key));
        }
    }

    fn remove(&mut self, id: i32) {
        if let Some(at) = self.items.iter().rposition(|n| n.0 == id) {
            self.items.swap_remove(at);
        }
    }

    fn clear(&mut self) {
        self.items.clear();
    }
}

pub(super) fn note_id(voice: u32) -> i32 {
    (voice & 0x7fff_ffff) as i32
}

pub(super) fn unit(v: f32) -> f64 {
    if v.is_finite() {
        f64::from(v.clamp(0.0, 1.0))
    } else {
        0.0
    }
}

pub(super) fn midi_velocity(v: f32) -> u8 {
    (unit(v) * 127.0).round().clamp(1.0, 127.0) as u8
}

pub(super) struct Expression {
    pub(super) bend: f32,
    range: f32,
    pub(super) pressure: f32,
    pub(super) keys: [Option<f32>; 128],
    midi_configured: bool,
}
impl Default for Expression {
    fn default() -> Self {
        Self {
            bend: 0.0,
            range: 2.0,
            pressure: 0.0,
            keys: [None; 128],
            midi_configured: false,
        }
    }
}
impl Expression {
    fn clear(&mut self) {
        self.bend = 0.0;
        self.pressure = 0.0;
        self.keys.fill(None);
    }
}
/// Latest desired values survive event-list pressure and plugin-internal resets.
/// Each dirty field is replayed once before ordinary events in the next chunk.
#[derive(Default)]
struct PendingExpression {
    bend: bool,
    range: bool,
    pressure: bool,
    keys: u128,
}

pub(super) fn midi_bend(semitones: f32, range: f32) -> [u8; 3] {
    let value = if range > 0.0 {
        (semitones / range).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let raw = 8192 + (value * if value < 0.0 { 8192.0 } else { 8191.0 }).round() as i32;
    [0xe0, (raw & 127) as u8, ((raw >> 7) & 127) as u8]
}

impl PendingExpression {
    fn any(&self) -> bool {
        self.bend || self.range || self.pressure || self.keys != 0
    }
}

/// A CLAP plugin hosted as a [`Device`].
///
/// # Threads
///
/// `ClapDevice` is the audio side of the plugin: `process`, `set_param`,
/// `param`, `reset` and `is_silent` are real-time safe and take no locks.
/// Everything else (`load`, `prepare`, state, `idle`) belongs to a control
/// thread. After the device is moved into an engine slot the control side
/// keeps a [`ClapHandle`], taken from [`ClapDevice::handle`], for `idle`,
/// state and parameter text.
pub struct ClapDevice {
    shared: Arc<PluginShared>,
    info: ClapPluginInfo,
    descriptor: &'static DeviceDescriptor,
    kind: DeviceKind,
    pub(super) dialect: Dialect,
    audio: AudioBuffers,
    pub(super) events: EventList,
    pending: PendingParams,
    voices: ActiveNotes,
    pub(super) expression: Expression,
    pending_expression: PendingExpression,
    pub(super) scoped: crate::scoped::Scoped,
    transport: clap_event_transport,
    steady_time: i64,
    sample_rate: f32,
    max_block: usize,
    /// Activated successfully and not failed since.
    ready: bool,
    /// `start_processing` has been called.
    started: bool,
    /// The last block ended with the plugin asking to sleep.
    sleeping: bool,
    /// Run `reset` after the current block (a panic was requested).
    reset_after_block: bool,
}

// SAFETY: the raw pointers inside (audio descriptors, event list) point into
// allocations the device owns, and the plugin behind `shared` is only called
// from one thread at a time on the audio side. CLAP allows the instance to
// move between audio threads between blocks.
unsafe impl Send for ClapDevice {}

impl ClapDevice {
    /// Loads and activates the plugin described by `info`.
    ///
    /// This loads the library (or joins an existing load of it), creates the
    /// plugin by id, queries its extensions, builds the `DeviceDescriptor`
    /// and activates it for `sample_rate` and blocks up to `max_block`
    /// frames. Control thread only: it loads libraries and allocates.
    pub fn load(
        info: &ClapPluginInfo,
        sample_rate: f32,
        max_block: usize,
    ) -> Result<ClapDevice, ClapError> {
        check_audio_config(sample_rate, max_block)?;
        let kind = match info.role {
            ClapRole::Instrument => DeviceKind::Instrument,
            ClapRole::AudioEffect => DeviceKind::Effect,
            other => {
                return Err(ClapError::Unsupported(format!(
                    "{} is a {other:?} plugin; only instruments and audio effects can be hosted",
                    info.name
                )));
            }
        };
        let module = ModuleHandle::open(&info.path)?;
        let shared = PluginShared::open(module, &info.id, &info.name)?;
        if shared.audio_out.is_empty() {
            return Err(ClapError::Unsupported(format!(
                "{} has no audio output port",
                info.name
            )));
        }
        if kind == DeviceKind::Effect && shared.audio_in.is_empty() {
            return Err(ClapError::Unsupported(format!(
                "{} is an effect but has no audio input port",
                info.name
            )));
        }
        let descriptor = descriptor::intern(DescriptorSpec::from_plugin(
            &shared,
            &info.id,
            &info.name,
            &info.description,
            kind,
        ));
        shared.activate(sample_rate, max_block)?;

        let shared = Arc::new(shared);
        let dialect = pick_dialect(&shared.note_in);
        let audio = build_audio(&shared, kind, max_block);
        let params = shared.table.len();
        let mut scoped = crate::scoped::Scoped::default();
        scoped.midi_enabled = dialect == Dialect::Midi
            && shared
                .note_in
                .first()
                .is_some_and(|port| port.supported_dialects & CLAP_NOTE_DIALECT_MIDI_MPE != 0);
        Ok(ClapDevice {
            events: EventList::with_capacity(BASE_EVENT_SLOTS + 2 * params, RELEASE_RESERVE),
            pending: PendingParams::new(params),
            voices: ActiveNotes::new(),
            expression: Expression::default(),
            pending_expression: PendingExpression::default(),
            scoped,
            transport: new_transport(),
            steady_time: 0,
            info: info.clone(),
            descriptor,
            kind,
            dialect,
            audio,
            sample_rate,
            max_block,
            ready: true,
            started: false,
            sleeping: false,
            reset_after_block: false,
            shared,
        })
    }

    /// A control-side handle that stays usable after the device moves into
    /// the engine. Clone it freely; drop every handle when the device is
    /// removed, because the plugin lives until the device and all handles
    /// are gone.
    pub fn handle(&self) -> ClapHandle {
        ClapHandle::new(Arc::clone(&self.shared), self.info.clone(), self.descriptor)
    }

    pub fn info(&self) -> &ClapPluginInfo {
        &self.info
    }

    /// The plugin exposes `clap.gui`; open it through [`ClapHandle::open_editor`].
    pub fn has_gui(&self) -> bool {
        self.shared.has_gui()
    }

    /// Services the plugin's main-thread requests. Call every UI frame on
    /// the control side. See [`ClapHandle::idle`].
    pub fn idle(&self) -> IdleReport {
        self.shared.idle()
    }

    /// Saves the plugin's complete state. Control thread only.
    pub fn save_state(&self) -> Result<Vec<u8>, ClapError> {
        self.shared.save_state()
    }

    /// Restores state from [`save_state`](Self::save_state). Control thread
    /// only. Parameter changes queued with `set_param` before this call are
    /// discarded so they cannot override the restored values; restore state
    /// before replaying any stored parameter values.
    pub fn load_state(&mut self, data: &[u8]) -> Result<(), ClapError> {
        self.shared.load_state(data)?;
        self.pending.clear();
        Ok(())
    }

    /// Switches the plugin between realtime and offline rendering where it
    /// supports `clap.render`. Returns false when it does not.
    pub fn set_offline_render(&self, offline: bool) -> bool {
        self.shared.set_render_offline(offline)
    }

    pub fn stats(&self) -> ClapStats {
        ClapStats::of(&self.shared)
    }

    // ---- audio side ------------------------------------------------------

    fn halt(&mut self) {
        if self.started {
            let _audio = RoleScope::audio();
            self.shared.stop_processing();
            self.started = false;
        }
    }

    fn restart(&mut self, sample_rate: f32, max_block: usize) {
        self.halt();
        match self.shared.activate(sample_rate, max_block) {
            Ok(()) => {
                self.ready = true;
                self.sample_rate = sample_rate;
                self.max_block = max_block;
                self.audio = build_audio(&self.shared, self.kind, max_block);
                self.voices.clear();
                self.replay_expression_after_reset();
                self.sleeping = false;
            }
            Err(e) => {
                log::warn!("{}: {e}", self.info.name);
                self.ready = false;
            }
        }
    }

    fn push_event(&mut self, e: &DeviceEvent, chunk_start: usize, chunk_len: usize) {
        let rel = (e.offset as usize)
            .saturating_sub(chunk_start)
            .min(chunk_len.saturating_sub(1));
        let time = rel as u32;
        let accepted = match e.kind {
            EventKind::ClearVoiceExpression { voice } => self.clear_scoped(time, voice),
            EventKind::VoiceExpression {
                voice,
                pitch,
                pressure,
                timbre,
            } => self.push_scoped(time, voice, pitch, pressure, timbre),
            EventKind::NoteOn {
                key,
                velocity,
                voice,
            } => {
                let key = key.min(127);
                let id = note_id(voice);
                let inherited = usize::from(self.expression.bend != 0.0)
                    + usize::from(self.expression.pressure != 0.0);
                let midi_extra = if self.dialect == Dialect::Midi && !self.scoped.midi_enabled {
                    usize::from(
                        self.expression.keys[usize::from(key)].is_some()
                            || self.pending_expression.keys & (1u128 << key) != 0,
                    ) + if self.expression.midi_configured && !self.pending_expression.range {
                        0
                    } else {
                        7
                    }
                } else {
                    0
                };
                if !self.events.has_room(
                    1 + midi_extra
                        + if self.dialect == Dialect::Clap {
                            inherited
                        } else {
                            0
                        },
                ) {
                    self.shared.dropped_events.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                if self.dialect == Dialect::Midi && !self.scoped.midi_enabled {
                    if !self.expression.midi_configured || self.pending_expression.range {
                        self.push_expression(
                            time,
                            EventKind::PitchBendRange {
                                semitones: self.expression.range,
                            },
                        );
                    }
                    if self.expression.keys[usize::from(key)].is_some()
                        || self.pending_expression.keys & (1u128 << key) != 0
                    {
                        self.events.push_midi(time, 0, [0xa0, key, 0], false);
                        self.pending_expression.keys &= !(1u128 << key);
                    }
                }
                if !self
                    .scoped
                    .note_on(id, key, self.expression.bend, self.expression.pressure)
                {
                    self.shared.dropped_events.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                let ok = match self.dialect {
                    Dialect::Clap => self.events.push_note(
                        CLAP_EVENT_NOTE_ON,
                        time,
                        id,
                        0,
                        0,
                        i16::from(key),
                        unit(velocity),
                        false,
                    ),
                    Dialect::Midi if self.scoped.midi_enabled => {
                        self.midi_member_on(time, id, key, velocity)
                    }
                    Dialect::Midi => {
                        self.events
                            .push_midi(time, 0, [0x90, key, midi_velocity(velocity)], false)
                    }
                    Dialect::None => return,
                };
                if ok {
                    self.voices.add(id, i16::from(key));
                    self.expression.keys[usize::from(key)] = None;
                    if self.dialect == Dialect::Clap {
                        if self.expression.bend != 0.0 {
                            self.events.push_expression(
                                time,
                                CLAP_NOTE_EXPRESSION_TUNING,
                                id,
                                i16::from(key),
                                f64::from(self.expression.bend),
                            );
                        }
                        if self.expression.pressure != 0.0 {
                            self.events.push_expression(
                                time,
                                CLAP_NOTE_EXPRESSION_PRESSURE,
                                id,
                                i16::from(key),
                                f64::from(self.expression.pressure),
                            );
                        }
                    }
                }
                ok
            }
            EventKind::NoteOff {
                key,
                velocity,
                voice,
            } => {
                let key = key.min(127);
                let id = note_id(voice);
                self.voices.remove(id);
                self.scoped.note_off(id);
                match self.dialect {
                    Dialect::Clap => self.events.push_note(
                        CLAP_EVENT_NOTE_OFF,
                        time,
                        id,
                        0,
                        0,
                        i16::from(key),
                        unit(velocity),
                        true,
                    ),
                    Dialect::Midi if self.scoped.midi_enabled => {
                        self.midi_member_off(time, id, key, velocity)
                    }
                    Dialect::Midi => self.events.push_midi(
                        time,
                        0,
                        [0x80, key, (unit(velocity) * 127.0).round() as u8],
                        true,
                    ),
                    Dialect::None => return,
                }
            }
            EventKind::Param { id, value } => {
                let Some(index) = self.shared.table.index_of(id) else {
                    return;
                };
                let Some(entry) = self.shared.table.get(index) else {
                    return;
                };
                if !value.is_finite() {
                    return;
                }
                let v = entry.clamp(f64::from(value));
                let ok = self
                    .events
                    .push_param(time, entry.id, entry.cookie, v, false);
                if ok {
                    self.shared.store_value(index, v);
                }
                ok
            }
            EventKind::PitchBend { .. }
            | EventKind::PitchBendRange { .. }
            | EventKind::ChannelPressure { .. }
            | EventKind::PolyPressure { .. }
            | EventKind::ResetExpression => self.push_expression(time, e.kind),
            EventKind::AllNotesOff => {
                self.release_all(time, false);
                true
            }
            EventKind::AllSoundOff => {
                self.expression.clear();
                self.pending_expression = PendingExpression::default();
                self.release_all(time, true);
                self.reset_after_block = true;
                true
            }
        };
        if !accepted {
            if let EventKind::NoteOn { voice, .. } = e.kind {
                self.scoped.note_off(note_id(voice));
            }
            self.shared.dropped_events.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn push_expression(&mut self, time: u32, kind: EventKind) -> bool {
        // Cache desired state even when the bounded plugin event list is full.
        // Invalid values never become pending and never reach plugin code.
        let kind = match kind {
            EventKind::PitchBend { semitones } if semitones.is_finite() => {
                let semitones = semitones.clamp(-120.0, 120.0);
                self.expression.bend = semitones;
                self.pending_expression.bend = true;
                EventKind::PitchBend { semitones }
            }
            EventKind::PitchBendRange { semitones } if semitones.is_finite() => {
                let semitones = semitones.clamp(0.0, 96.0);
                self.expression.range = semitones;
                self.pending_expression.range = true;
                EventKind::PitchBendRange { semitones }
            }
            EventKind::ChannelPressure { value } if value.is_finite() => {
                let value = value.clamp(0.0, 1.0);
                self.expression.pressure = value;
                self.pending_expression.pressure = true;
                EventKind::ChannelPressure { value }
            }
            EventKind::PolyPressure { key, value } if key < 128 && value.is_finite() => {
                let value = value.clamp(0.0, 1.0);
                self.expression.keys[usize::from(key)] = Some(value);
                self.pending_expression.keys |= 1u128 << key;
                EventKind::PolyPressure { key, value }
            }
            EventKind::ResetExpression => {
                self.pending_expression.keys |= self.expression_key_mask();
                self.expression.clear();
                self.pending_expression.bend = true;
                self.pending_expression.pressure = true;
                kind
            }
            _ => return false,
        };
        self.retry_expression(time, kind)
    }

    fn expression_key_mask(&self) -> u128 {
        self.expression
            .keys
            .iter()
            .enumerate()
            .fold(0, |mask, (key, value)| {
                mask | if value.is_some() { 1u128 << key } else { 0 }
            })
    }

    fn retry_expression(&mut self, time: u32, kind: EventKind) -> bool {
        if !self.emit_expression(time, kind) {
            return false;
        }
        match kind {
            EventKind::PitchBend { .. } => self.pending_expression.bend = false,
            EventKind::PitchBendRange { .. } => self.pending_expression.range = false,
            EventKind::ChannelPressure { .. } => {
                self.pending_expression.pressure = false;
                // Native channel pressure broadcasts then reapplies every key override.
                if self.dialect == Dialect::Clap {
                    self.pending_expression.keys = 0;
                }
            }
            EventKind::PolyPressure { key, .. } => self.pending_expression.keys &= !(1u128 << key),
            EventKind::ResetExpression => {
                self.pending_expression.bend = false;
                self.pending_expression.pressure = false;
                self.pending_expression.keys = 0;
            }
            _ => {}
        }
        true
    }

    fn replay_expression_after_reset(&mut self) {
        self.expression.midi_configured = false;
        self.pending_expression.range = self.dialect == Dialect::Midi;
        self.pending_expression.bend = true;
        self.pending_expression.pressure = true;
        self.pending_expression.keys |= self.expression_key_mask();
    }

    fn flush_expression(&mut self) {
        if self.pending_expression.range {
            self.retry_expression(
                0,
                EventKind::PitchBendRange {
                    semitones: self.expression.range,
                },
            );
        }
        if self.pending_expression.bend {
            self.retry_expression(
                0,
                EventKind::PitchBend {
                    semitones: self.expression.bend,
                },
            );
        }
        if self.pending_expression.pressure {
            self.retry_expression(
                0,
                EventKind::ChannelPressure {
                    value: self.expression.pressure,
                },
            );
        }
        let mut keys = self.pending_expression.keys;
        while keys != 0 {
            let key = keys.trailing_zeros() as u8;
            keys &= keys - 1;
            self.retry_expression(
                0,
                EventKind::PolyPressure {
                    key,
                    value: self.expression.keys[usize::from(key)].unwrap_or(0.0),
                },
            );
        }
    }

    fn emit_expression(&mut self, time: u32, kind: EventKind) -> bool {
        if self.dialect == Dialect::Midi && self.scoped.midi_enabled {
            return self.midi_mpe_global(time, kind);
        }
        let count = match self.dialect {
            Dialect::None => return true,
            Dialect::Clap => match kind {
                EventKind::PitchBendRange { .. } => 0,
                EventKind::ChannelPressure { .. } => {
                    1 + self.expression.keys.iter().filter(|k| k.is_some()).count()
                }
                EventKind::ResetExpression => 2,
                _ => 1,
            },
            Dialect::Midi => match kind {
                EventKind::PitchBendRange { .. } => 7,
                EventKind::ResetExpression => {
                    2 + self.pending_expression.keys.count_ones() as usize
                }
                _ => 1,
            },
        };
        let configure = self.dialect == Dialect::Midi
            && (!self.expression.midi_configured || self.pending_expression.range)
            && !matches!(kind, EventKind::PitchBendRange { .. });
        if !self
            .events
            .has_room(count + if configure { 7 } else { 0 } + self.scoped_reapply_count())
        {
            return false;
        }
        if configure {
            self.retry_expression(
                time,
                EventKind::PitchBendRange {
                    semitones: self.expression.range,
                },
            );
        }
        match self.dialect {
            Dialect::Clap => {
                // CLAP wildcard addressing includes released voices still ringing
                // inside the plugin; NOTE_OFF is not the end of a voice's lifetime.
                match kind {
                    EventKind::PitchBend { semitones } => {
                        self.events.push_expression(
                            time,
                            CLAP_NOTE_EXPRESSION_TUNING,
                            -1,
                            -1,
                            f64::from(semitones),
                        );
                    }
                    EventKind::ChannelPressure { value } => {
                        self.events.push_expression(
                            time,
                            CLAP_NOTE_EXPRESSION_PRESSURE,
                            -1,
                            -1,
                            f64::from(value),
                        );
                        for (key, pressure) in self.expression.keys.iter().enumerate() {
                            if let Some(pressure) = pressure {
                                let combined = value + pressure - value * pressure;
                                self.events.push_expression(
                                    time,
                                    CLAP_NOTE_EXPRESSION_PRESSURE,
                                    -1,
                                    key as i16,
                                    f64::from(combined),
                                );
                            }
                        }
                    }
                    EventKind::PolyPressure { key, value } => {
                        let channel = self.expression.pressure;
                        let combined = channel + value - channel * value;
                        self.events.push_expression(
                            time,
                            CLAP_NOTE_EXPRESSION_PRESSURE,
                            -1,
                            i16::from(key),
                            f64::from(combined),
                        );
                    }
                    EventKind::ResetExpression => {
                        self.events
                            .push_expression(time, CLAP_NOTE_EXPRESSION_TUNING, -1, -1, 0.0);
                        self.events.push_expression(
                            time,
                            CLAP_NOTE_EXPRESSION_PRESSURE,
                            -1,
                            -1,
                            0.0,
                        );
                    }
                    _ => {}
                }
            }
            Dialect::Midi => match kind {
                EventKind::PitchBend { semitones } => {
                    self.events.push_midi(
                        time,
                        0,
                        midi_bend(semitones, self.expression.range),
                        false,
                    );
                }
                EventKind::PitchBendRange { semitones } => {
                    // Reserve the entire RPN transaction before selecting a controller.
                    let cents = (semitones * 100.0).round() as u16;
                    for (cc, value) in [
                        (101, 0),
                        (100, 0),
                        (6, (cents / 100) as u8),
                        (38, (cents % 100) as u8),
                        (101, 127),
                        (100, 127),
                    ] {
                        self.events.push_midi(time, 0, [0xb0, cc, value], false);
                    }
                    self.events.push_midi(
                        time,
                        0,
                        midi_bend(self.expression.bend, semitones),
                        false,
                    );
                    self.expression.midi_configured = true;
                }
                EventKind::ChannelPressure { value } => {
                    self.events
                        .push_midi(time, 0, [0xd0, (value * 127.0).round() as u8, 0], false);
                }
                EventKind::PolyPressure { key, value } => {
                    self.events.push_midi(
                        time,
                        0,
                        [0xa0, key, (value * 127.0).round() as u8],
                        false,
                    );
                }
                EventKind::ResetExpression => {
                    self.events.push_midi(time, 0, [0xe0, 0, 64], false);
                    self.events.push_midi(time, 0, [0xd0, 0, 0], false);
                    let mut keys = self.pending_expression.keys;
                    while keys != 0 {
                        let key = keys.trailing_zeros() as u8;
                        keys &= keys - 1;
                        self.events.push_midi(time, 0, [0xa0, key, 0], false);
                    }
                }
                _ => {}
            },
            Dialect::None => {}
        }
        self.reapply_scoped(time);
        true
    }

    /// Releases (or chokes) every held note. The wildcard form goes first: it
    /// is the one that covers notes the host is not tracking, so it must not
    /// lose its slot to the per-note releases that follow.
    fn release_all(&mut self, time: u32, choke: bool) {
        self.release_mpe_members(time, choke);
        let type_ = if choke {
            CLAP_EVENT_NOTE_CHOKE
        } else {
            CLAP_EVENT_NOTE_OFF
        };
        match self.dialect {
            Dialect::Clap => {
                self.events
                    .push_note(type_, time, -1, -1, -1, -1, 0.0, true);
                for &(id, key) in &self.voices.items {
                    self.events.push_note(type_, time, id, 0, 0, key, 0.0, true);
                }
            }
            Dialect::Midi => {
                // CC 123 all notes off, CC 120 all sound off.
                let cc = if choke { 120 } else { 123 };
                self.events.push_midi(time, 0, [0xB0, cc, 0], true);
                for &(_, key) in &self.voices.items {
                    self.events.push_midi(time, 0, [0x80, key as u8, 0], true);
                }
            }
            Dialect::None => {}
        }
        self.voices.clear();
        for entry in &mut self.scoped.entries {
            entry.held = false;
        }
        if choke {
            self.scoped.reset();
        }
    }

    /// Sends one parameter value outside of `process`. False when the plugin
    /// has no `flush`, in which case the value must go through `pending`.
    fn flush_param(&mut self, entry: &ParamEntry, value: f64) -> bool {
        if !self.shared.has_flush() {
            return false;
        }
        self.events.clear();
        self.events
            .push_param(0, entry.id, entry.cookie, value, true);
        let input = events::input_events(&self.events);
        let output = events::output_events(&self.shared);
        {
            let _audio = RoleScope::audio();
            self.shared.flush(&input, &output);
        }
        self.events.clear();
        true
    }

    fn fill_transport(&mut self, ctx: &ProcessContext, frames_into_block: usize) {
        let mut flags = CLAP_TRANSPORT_HAS_TEMPO | CLAP_TRANSPORT_HAS_BEATS_TIMELINE;
        if ctx.playing {
            flags |= CLAP_TRANSPORT_IS_PLAYING;
        }
        let beats = ctx.beat_position
            + frames_into_block as f64 / f64::from(ctx.samples_per_beat()).max(1.0);
        self.transport.flags = flags;
        self.transport.tempo = ctx.tempo_bpm;
        self.transport.song_pos_beats = (beats * CLAP_BEATTIME_FACTOR as f64) as i64;
    }

    /// Runs one chunk of at most `max_block` frames. Returns the CLAP status.
    fn run_chunk(
        &mut self,
        ctx: &ProcessContext,
        left: &mut [f32],
        right: &mut [f32],
        chunk_start: usize,
    ) -> i32 {
        let frames = left.len();
        self.audio.clear(frames);
        if self.kind == DeviceKind::Effect {
            self.audio.load_main_input(left, right);
        }
        self.fill_transport(ctx, chunk_start);

        let in_events = events::input_events(&self.events);
        let out_events = events::output_events(&self.shared);
        let transport: *const clap_event_transport = &self.transport;
        let (audio_inputs, audio_inputs_count, audio_outputs, audio_outputs_count) =
            self.audio.bind();
        let process = clap_process {
            steady_time: self.steady_time,
            frames_count: frames as u32,
            transport,
            audio_inputs,
            audio_outputs,
            audio_inputs_count,
            audio_outputs_count,
            in_events: &in_events,
            out_events: &out_events,
        };
        let status = self.shared.process(&process);
        self.steady_time = self.steady_time.wrapping_add(frames as i64);
        status
    }

    /// Output when the plugin cannot run: silence from an instrument, the
    /// untouched input from an effect.
    fn bypass(&self, left: &mut [f32], right: &mut [f32]) {
        if self.kind == DeviceKind::Instrument {
            left.fill(0.0);
            right.fill(0.0);
        }
    }
}

fn check_audio_config(sample_rate: f32, max_block: usize) -> Result<(), ClapError> {
    if !(sample_rate.is_finite() && sample_rate > 0.0) {
        return Err(ClapError::InvalidArgument(format!(
            "sample rate {sample_rate}"
        )));
    }
    if max_block == 0 {
        return Err(ClapError::InvalidArgument("max_block is 0".into()));
    }
    Ok(())
}

fn new_transport() -> clap_event_transport {
    clap_event_transport {
        header: clap_event_header {
            size: size_of::<clap_event_transport>() as u32,
            time: 0,
            space_id: CLAP_CORE_EVENT_SPACE_ID,
            type_: CLAP_EVENT_TRANSPORT,
            flags: 0,
        },
        flags: 0,
        song_pos_beats: 0,
        song_pos_seconds: 0,
        tempo: 120.0,
        tempo_inc: 0.0,
        loop_start_beats: 0,
        loop_end_beats: 0,
        loop_start_seconds: 0,
        loop_end_seconds: 0,
        bar_start: 0,
        bar_number: 0,
        tsig_num: 4,
        tsig_denom: 4,
    }
}

fn build_audio(shared: &PluginShared, kind: DeviceKind, max_block: usize) -> AudioBuffers {
    let main =
        |ports: &[crate::instance::PortInfo]| ports.iter().position(|p| p.is_main).unwrap_or(0);
    let main_in =
        (kind == DeviceKind::Effect && !shared.audio_in.is_empty()).then(|| main(&shared.audio_in));
    AudioBuffers::new(
        &shared.audio_in,
        &shared.audio_out,
        main_in,
        main(&shared.audio_out),
        max_block,
    )
}

impl Device for ClapDevice {
    fn descriptor(&self) -> &'static DeviceDescriptor {
        self.descriptor
    }

    /// Re-activates the plugin when the rate or block size changed, or when
    /// the plugin asked for a restart. Does nothing when neither did.
    fn prepare(&mut self, sample_rate: f32, max_block: usize) {
        if check_audio_config(sample_rate, max_block).is_err() {
            return;
        }
        let restart = self.shared.host.shared.is_raised(F_RESTART);
        if self.ready && !restart && sample_rate == self.sample_rate && max_block == self.max_block
        {
            return;
        }
        self.restart(sample_rate, max_block);
    }

    fn reset(&mut self) {
        self.voices.clear();
        self.scoped.reset();
        self.replay_expression_after_reset();
        self.events.clear();
        self.sleeping = false;
        if self.ready && self.started {
            let _audio = RoleScope::audio();
            self.shared.reset();
        }
    }

    /// Records the value and hands it to the plugin: with the next block when
    /// the device is processing, at once (through `params.flush`) when it is
    /// not, so a stopped transport or a sleeping plugin still follows the
    /// knob and its saved state stays current.
    fn set_param(&mut self, id: ParamId, value: f32) {
        let Some(index) = self.shared.table.index_of(id) else {
            return;
        };
        let Some(entry) = self.shared.table.get(index).copied() else {
            return;
        };
        if !value.is_finite() {
            return;
        }
        let v = entry.clamp(f64::from(value));
        self.shared.store_value(index, v);
        let idle = !self.started || self.sleeping;
        if self.ready && idle && self.flush_param(&entry, v) {
            return;
        }
        self.pending.set(index, v);
    }

    fn param(&self, id: ParamId) -> f32 {
        self.shared
            .table
            .index_of(id)
            .and_then(|i| self.shared.value(i))
            .unwrap_or(0.0)
    }

    fn process(
        &mut self,
        ctx: &ProcessContext,
        events: &[DeviceEvent],
        left: &mut [f32],
        right: &mut [f32],
    ) {
        self.mark_scoped_block(events);
        let frames = left.len().min(right.len());
        if frames == 0 {
            return;
        }
        if !self.ready {
            self.bypass(left, right);
            return;
        }
        let _audio = RoleScope::audio();
        if !self.started {
            if self.shared.start_processing() {
                self.started = true;
            } else {
                self.ready = false;
                self.shared.process_errors.fetch_add(1, Ordering::Relaxed);
                self.bypass(left, right);
                return;
            }
        }
        self.shared.host.shared.take(F_PROCESS | F_FLUSH);

        let mut next_event = 0usize;
        let mut pos = 0usize;
        let mut status = CLAP_PROCESS_SLEEP;
        let mut peak = 0.0f32;
        while pos < frames {
            let n = (frames - pos).min(self.max_block);
            self.events.clear();
            self.flush_expression();
            self.flush_scoped();
            if pos == 0 {
                self.pending
                    .drain_into(&self.shared.table, &mut self.events);
            }
            // The last chunk also takes events past the end of the block (a
            // contract violation), at its final frame, so a note-off is never
            // lost.
            let last_chunk = pos + n == frames;
            while next_event < events.len()
                && (last_chunk || (events[next_event].offset as usize) < pos + n)
            {
                let e = events[next_event];
                self.push_event(&e, pos, n);
                next_event += 1;
            }
            let (l, r) = (&mut left[pos..pos + n], &mut right[pos..pos + n]);
            status = self.run_chunk(ctx, l, r, pos);
            if status == CLAP_PROCESS_ERROR {
                self.shared.process_errors.fetch_add(1, Ordering::Relaxed);
                // Discard the plugin's output for this chunk.
                if self.kind == DeviceKind::Instrument {
                    l.fill(0.0);
                    r.fill(0.0);
                }
            } else {
                peak = peak.max(self.audio.store_main_output(l, r));
            }
            pos += n;
        }

        if self.reset_after_block {
            self.reset_after_block = false;
            self.shared.reset();
            self.replay_expression_after_reset();
        }
        self.sleeping = match status {
            CLAP_PROCESS_SLEEP => true,
            CLAP_PROCESS_CONTINUE_IF_NOT_QUIET => peak <= QUIET_PEAK,
            _ => false,
        };
    }

    fn latency(&self) -> u32 {
        self.shared.latency_frames()
    }

    /// `None` when the plugin has no `tail` extension or reports an
    /// infinite tail.
    fn tail_seconds(&self) -> Option<f32> {
        self.shared.tail_seconds()
    }

    /// True after the plugin returned `CLAP_PROCESS_SLEEP` (or went quiet
    /// after `CONTINUE_IF_NOT_QUIET`), until it asks to be processed again.
    fn is_silent(&self) -> bool {
        self.sleeping
            && !self.pending_expression.any()
            && !self.scoped.entries.iter().any(|e| e.dirty)
            && !self.shared.host.shared.is_raised(F_PROCESS | F_FLUSH)
    }
}

impl std::fmt::Debug for ClapDevice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapDevice")
            .field("plugin", &self.info.id)
            .field("sample_rate", &self.sample_rate)
            .field("max_block", &self.max_block)
            .field("ready", &self.ready)
            .finish_non_exhaustive()
    }
}

impl Drop for ClapDevice {
    fn drop(&mut self) {
        // `stop_processing` before the plugin is deactivated and destroyed
        // (which happens when the last `Arc` goes, possibly in a handle).
        self.halt();
    }
}

#[cfg(test)]
#[path = "expression_tests.rs"]
mod expression_tests;
