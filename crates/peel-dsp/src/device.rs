use std::sync::Arc;

use crate::param::{ParamDescriptor, ParamId};
use crate::sample::SampleBuffer;

/// Whether a device generates sound from notes or processes audio.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DeviceKind {
    Instrument,
    Effect,
}

/// Static description of a native device type.
///
/// `type_id` and every parameter `id`/`key` are stable identities stored in
/// project files. Never renumber or rename them; add new ones and bump
/// `version` with a migration instead.
#[derive(Debug)]
pub struct DeviceDescriptor {
    /// Stable identifier, e.g. `peel.synth.poly`.
    pub type_id: &'static str,
    /// Short display name, e.g. `Poly`.
    pub name: &'static str,
    /// One-line description for the browser tooltip.
    pub description: &'static str,
    /// Browser grouping, e.g. `Synth`, `Drums`, `Keys`, `EQ`, `Dynamics`.
    pub category: &'static str,
    pub kind: DeviceKind,
    /// DSP algorithm version. Changing how a device sounds requires a bump.
    pub version: u32,
    pub params: &'static [ParamDescriptor],
}

impl DeviceDescriptor {
    pub fn param(&self, id: ParamId) -> Option<&'static ParamDescriptor> {
        self.params.iter().find(|p| p.id == id)
    }

    pub fn param_by_key(&self, key: &str) -> Option<&'static ParamDescriptor> {
        self.params.iter().find(|p| p.key == key)
    }
}

/// Per-block transport and timing context.
#[derive(Clone, Copy, Debug)]
pub struct ProcessContext {
    pub sample_rate: f32,
    /// Tempo at the start of the block.
    pub tempo_bpm: f64,
    /// Musical position (quarter notes) at the first frame of the block.
    pub beat_position: f64,
    pub playing: bool,
}

impl ProcessContext {
    pub fn new(sample_rate: f32) -> ProcessContext {
        ProcessContext {
            sample_rate,
            tempo_bpm: 120.0,
            beat_position: 0.0,
            playing: false,
        }
    }

    /// Length of one quarter note in samples at the current tempo.
    pub fn samples_per_beat(&self) -> f32 {
        (self.sample_rate as f64 * 60.0 / self.tempo_bpm.max(1.0)) as f32
    }
}

/// What happened. Voices are identified by `voice`, not by key, so repeated
/// or overlapping pattern instances cannot steal each other's note-offs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EventKind {
    /// `velocity` is 0..=1.
    NoteOn { key: u8, velocity: f32, voice: u32 },
    /// `velocity` is the release velocity, 0..=1.
    NoteOff { key: u8, velocity: f32, voice: u32 },
    /// Sample-accurate parameter change (plain value).
    Param { id: ParamId, value: f32 },
    /// Live pitch displacement in semitones (finite, -120..=120).
    PitchBend { semitones: f32 },
    /// MIDI pitch-wheel sensitivity. Native DSP already receives actual semitones.
    PitchBendRange { semitones: f32 },
    /// Channel aftertouch, 0..=1. Does not rewrite a saved parameter.
    ChannelPressure { value: f32 },
    /// Polyphonic aftertouch for sounding voices of `key`, 0..=1.
    PolyPressure { key: u8, value: f32 },
    /// Complete expression for one voice. Overrides global expression for that voice.
    /// Pitch is semitones (-120..120), pressure is 0..1 and timbre is 0..1 (neutral .5).
    VoiceExpression {
        voice: u32,
        pitch: f32,
        pressure: f32,
        timbre: f32,
    },
    /// Relinquish one voice's override and resume shared expression without retriggering.
    ClearVoiceExpression { voice: u32 },
    /// MIDI control change: `cc` is the controller number (0..=127) and
    /// `value` is 0..=1 (127 is 1.0). Hosted plugins receive it as MIDI
    /// (CC64 is the sustain pedal); built-in devices ignore it.
    Controller { cc: u8, value: f32 },
    /// Clear transient pitch and pressure without ending sequence voices.
    ResetExpression,
    /// Release every sounding voice normally (tails continue).
    AllNotesOff,
    /// Silence immediately and clear tails (panic).
    AllSoundOff,
}

/// An event inside the current block. `offset < frames` always holds.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceEvent {
    pub offset: u32,
    pub kind: EventKind,
}

/// A first-party instrument or effect.
pub trait Device: Send {
    fn descriptor(&self) -> &'static DeviceDescriptor;

    /// Allocates resources for the sample rate and maximum block size.
    /// Called off the audio thread before the device is installed, and again
    /// (off the audio thread) whenever either value changes.
    fn prepare(&mut self, sample_rate: f32, max_block: usize);

    /// Clears voices, delay lines and envelopes without allocating.
    fn reset(&mut self);

    /// Sets a parameter's plain value. Unknown ids are ignored. Values are
    /// clamped to the descriptor range by the device.
    fn set_param(&mut self, id: ParamId, value: f32);

    /// Current plain value of a parameter (descriptor default if unknown).
    fn param(&self, id: ParamId) -> f32;

    /// Renders one block.
    ///
    /// `left` and `right` have equal length `frames <= max_block`. An
    /// instrument overwrites both buffers with its output. An effect
    /// processes them in place. `events` are sorted by `offset`.
    fn process(
        &mut self,
        ctx: &ProcessContext,
        events: &[DeviceEvent],
        left: &mut [f32],
        right: &mut [f32],
    );

    /// Renders one block with a sidechain (key) signal of the same length,
    /// for devices that react to another track (a compressor ducking under
    /// the kick). Devices without a sidechain input ignore the key.
    fn process_keyed(
        &mut self,
        ctx: &ProcessContext,
        events: &[DeviceEvent],
        left: &mut [f32],
        right: &mut [f32],
        key_left: &[f32],
        key_right: &[f32],
    ) {
        let _ = (key_left, key_right);
        self.process(ctx, events, left, right);
    }

    /// Gain reduction in dB (zero or positive) at the end of the last block,
    /// for dynamics processors that show a meter. Must be cheap.
    fn reduction_db(&self) -> Option<f32> {
        None
    }

    /// Processing latency in samples introduced by lookahead.
    fn latency(&self) -> u32 {
        0
    }

    /// Tail length in seconds after input stops (reverb, delay). `None` means
    /// infinite or unknown.
    fn tail_seconds(&self) -> Option<f32> {
        Some(0.0)
    }

    /// True when the device is producing silence and holds no sounding
    /// voices, letting the engine skip work. Must be cheap.
    fn is_silent(&self) -> bool {
        false
    }

    /// Replaces the device's sample (samplers). Returns the previous sample,
    /// which the engine hands back to a control thread for release, so this
    /// never frees memory on the audio thread. Devices that do not use
    /// samples return the argument unchanged.
    fn set_sample(&mut self, sample: Option<Arc<SampleBuffer>>) -> Option<Arc<SampleBuffer>> {
        sample
    }
}

/// Applies every descriptor default to a freshly created device.
pub fn apply_defaults(device: &mut dyn Device) {
    let desc = device.descriptor();
    for p in desc.params {
        device.set_param(p.id, p.default);
    }
}
