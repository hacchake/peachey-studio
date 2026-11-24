//! MIDI controller settings and a bounded callback-to-control-thread mailbox.

use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use peel_document::Id;
use peel_dsp::ParamDescriptor;
use serde::{Deserialize, Serialize};

use super::Session;

const OMNI: u8 = 16;
const CHANNELS: usize = 16;
const CONTROLLERS: usize = 128;
const MAX_MAPPINGS: usize = 256;
const GESTURE_IDLE: Duration = Duration::from_millis(350);

/// A controller target uses document identities, never a device's engine slot.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MidiControlTarget {
    DeviceParam { device: Id, key: String },
    TrackGain { track: Id },
    TrackPan { track: Id },
    ChannelGain { channel: Id },
    ChannelPan { channel: Id },
}

/// One CC mapping. Ranges are plain values; parameter tapers remain intact.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MidiMapping {
    pub controller: u8,
    pub channel: Option<u8>,
    pub target: MidiControlTarget,
    pub min: f32,
    pub max: f32,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MidiLearnStatus {
    #[default]
    Idle,
    Waiting {
        target: MidiControlTarget,
    },
    Learned {
        mapping: MidiMapping,
    },
    Error {
        message: String,
    },
}

/// A single pending value for every MIDI channel/CC pair. Writers only use
/// atomics; floods coalesce without allocating or blocking on the UI.
pub(super) struct ControlMailbox {
    filter: AtomicU8,
    values: [AtomicU16; CHANNELS * CONTROLLERS],
    learning: AtomicBool,
    learned: AtomicU32,
}

impl Default for ControlMailbox {
    fn default() -> Self {
        Self {
            filter: AtomicU8::new(OMNI),
            values: [const { AtomicU16::new(0) }; CHANNELS * CONTROLLERS],
            learning: AtomicBool::new(false),
            learned: AtomicU32::new(0),
        }
    }
}

impl ControlMailbox {
    pub(super) fn accepts(&self, channel: u8) -> bool {
        let filter = self.filter.load(Ordering::Relaxed);
        channel < CHANNELS as u8 && (filter == OMNI || filter == channel)
    }

    pub(super) fn push(&self, channel: u8, controller: u8, value: u8) {
        if !self.accepts(channel) || !valid_controller(controller) || value > 127 {
            return;
        }
        if self.learning.load(Ordering::Relaxed) {
            let packed =
                1 + (u32::from(channel) << 16 | u32::from(controller) << 8 | u32::from(value));
            let _ = self
                .learned
                .compare_exchange(0, packed, Ordering::Release, Ordering::Relaxed);
        }
        self.values[usize::from(channel) * CONTROLLERS + usize::from(controller)]
            .store(u16::from(value) + 1, Ordering::Release);
    }

    fn clear(&self) {
        for value in &self.values {
            value.store(0, Ordering::Relaxed);
        }
        self.learned.store(0, Ordering::Relaxed);
    }
}

fn valid_controller(controller: u8) -> bool {
    controller < 128 && !matches!(controller, 64 | 120 | 123)
}

pub(super) struct MidiControls {
    mappings: Vec<MidiMapping>,
    learn: MidiLearnStatus,
    error: Option<String>,
    gesture: u64,
    last_control: Option<Instant>,
}

impl Default for MidiControls {
    fn default() -> Self {
        Self {
            mappings: Vec::new(),
            learn: MidiLearnStatus::Idle,
            error: None,
            gesture: 0x4d49_4449_0000_0000,
            last_control: None,
        }
    }
}

#[derive(Clone, Copy)]
enum Range {
    Parameter(ParamDescriptor),
    Gain,
    Pan,
}

impl Range {
    fn bounds(self) -> (f32, f32) {
        match self {
            Self::Parameter(p) => (p.min, p.max),
            Self::Gain => (-60.0, 12.0),
            Self::Pan => (-1.0, 1.0),
        }
    }

    fn value(self, mapping: &MidiMapping, value: u8) -> f32 {
        // Preserve the exact requested endpoints, including decimal values
        // that normalized log conversion would otherwise round slightly.
        match value {
            0 => mapping.min,
            127 => mapping.max,
            value => {
                let n = f32::from(value) / 127.0;
                match self {
                    Self::Parameter(p) => {
                        let low = p.to_normalized(mapping.min);
                        let high = p.to_normalized(mapping.max);
                        p.from_normalized(low + n * (high - low))
                    }
                    _ => mapping.min + n * (mapping.max - mapping.min),
                }
            }
        }
    }
}

impl Session {
    pub fn midi_control_mappings(&self) -> &[MidiMapping] {
        &self.io.controls.mappings
    }

    /// Validates the entire replacement before changing any setting.
    pub fn set_midi_control_mappings(&mut self, mappings: Vec<MidiMapping>) -> Result<(), String> {
        if mappings.len() > MAX_MAPPINGS {
            return Err(format!(
                "At most {MAX_MAPPINGS} MIDI mappings are supported"
            ));
        }
        for (index, mapping) in mappings.iter().enumerate() {
            self.validate_midi_mapping(mapping)?;
            if mappings[..index]
                .iter()
                .any(|m| m.controller == mapping.controller && m.channel == mapping.channel)
            {
                return Err("Each MIDI channel/CC can have only one mapping".into());
            }
        }
        self.io.controls.mappings = mappings;
        self.io.controls.error = None;
        self.io.bridge.controls.clear();
        Ok(())
    }

    pub fn add_midi_control_mapping(&mut self, mapping: MidiMapping) -> Result<(), String> {
        self.validate_midi_mapping(&mapping)?;
        let same_source =
            |m: &MidiMapping| m.controller == mapping.controller && m.channel == mapping.channel;
        if self.io.controls.mappings.len() >= MAX_MAPPINGS
            && !self.io.controls.mappings.iter().any(same_source)
        {
            return Err(format!(
                "At most {MAX_MAPPINGS} MIDI mappings are supported"
            ));
        }
        // Replace this exact source only. Channel-specific mappings take
        // precedence over omni, leaving the omni assignment on other channels.
        self.io.controls.mappings.retain(|m| !same_source(m));
        self.io.controls.mappings.push(mapping);
        self.io.controls.error = None;
        Ok(())
    }

    pub fn remove_midi_control_mapping(&mut self, index: usize) -> bool {
        if index >= self.io.controls.mappings.len() {
            return false;
        }
        self.io.controls.mappings.remove(index);
        self.io.controls.error = None;
        true
    }

    pub fn midi_channel_filter(&self) -> Option<u8> {
        let filter = self.io.bridge.controls.filter.load(Ordering::Relaxed);
        (filter < OMNI).then_some(filter)
    }

    /// Selects a MIDI channel (0..15), or accepts all channels with `None`.
    /// Flushes the old notes and pedal before later events use the filter.
    pub fn set_midi_channel_filter(&mut self, channel: Option<u8>) -> Result<(), String> {
        if channel.is_some_and(|ch| ch >= OMNI) {
            return Err("MIDI channel must be between 0 and 15".into());
        }
        if self.midi_channel_filter() != channel {
            self.io
                .bridge
                .controls
                .filter
                .store(channel.unwrap_or(OMNI), Ordering::Relaxed);
            self.io.bridge.release_all();
            self.io.bridge.controls.clear();
        }
        Ok(())
    }

    /// Waits for the next accepted, non-reserved controller message.
    pub fn begin_midi_learn(&mut self, target: MidiControlTarget) -> Result<(), String> {
        self.midi_target_range(&target)?;
        self.io.bridge.controls.clear();
        self.io.controls.learn = MidiLearnStatus::Waiting { target };
        self.io.controls.error = None;
        self.io
            .bridge
            .controls
            .learning
            .store(true, Ordering::Release);
        Ok(())
    }

    pub fn cancel_midi_learn(&mut self) {
        self.io
            .bridge
            .controls
            .learning
            .store(false, Ordering::Release);
        self.io.bridge.controls.learned.store(0, Ordering::Relaxed);
        self.io.controls.learn = MidiLearnStatus::Idle;
    }

    pub fn midi_learn_status(&self) -> &MidiLearnStatus {
        &self.io.controls.learn
    }

    pub fn midi_control_error(&self) -> Option<&str> {
        self.io.controls.error.as_deref()
    }

    /// Sends an internal keyboard or software controller event through the
    /// same path as a hardware port. Hardware MIDI need not be enabled.
    pub fn handle_midi_event(&mut self, event: super::MidiEvent) {
        self.io.bridge.handle(&event);
    }

    fn midi_target_range(&self, target: &MidiControlTarget) -> Result<Range, String> {
        match target {
            MidiControlTarget::DeviceParam { device, key } => {
                let instance = crate::find_device(self.project(), *device)
                    .ok_or_else(|| "MIDI mapping device no longer exists".to_string())?;
                let descriptor = peel_devices::descriptor(&instance.type_id)
                    .ok_or_else(|| "MIDI mapping device is unavailable".to_string())?;
                let param = descriptor
                    .param_by_key(key)
                    .ok_or_else(|| format!("Unknown MIDI mapping parameter {key}"))?;
                if !param.automatable {
                    return Err("This parameter does not support MIDI control".into());
                }
                Ok(Range::Parameter(*param))
            }
            MidiControlTarget::TrackGain { track } | MidiControlTarget::TrackPan { track } => {
                self.project()
                    .mixer
                    .track(*track)
                    .ok_or("MIDI mapping track no longer exists")?;
                Ok(if matches!(target, MidiControlTarget::TrackGain { .. }) {
                    Range::Gain
                } else {
                    Range::Pan
                })
            }
            MidiControlTarget::ChannelGain { channel }
            | MidiControlTarget::ChannelPan { channel } => {
                self.project()
                    .channel(*channel)
                    .ok_or("MIDI mapping channel no longer exists")?;
                Ok(if matches!(target, MidiControlTarget::ChannelGain { .. }) {
                    Range::Gain
                } else {
                    Range::Pan
                })
            }
        }
    }

    fn validate_midi_mapping(&self, mapping: &MidiMapping) -> Result<Range, String> {
        if !valid_controller(mapping.controller) {
            return Err(
                "MIDI CC must be 0–127; 64, 120 and 123 are reserved for sustain and panic".into(),
            );
        }
        if mapping.channel.is_some_and(|ch| ch >= OMNI) {
            return Err("MIDI channel must be between 0 and 15".into());
        }
        let range = self.midi_target_range(&mapping.target)?;
        let (min, max) = range.bounds();
        if !mapping.min.is_finite()
            || !mapping.max.is_finite()
            || mapping.min < min
            || mapping.max > max
            || mapping.min > mapping.max
        {
            return Err(format!(
                "MIDI mapping range must be between {min} and {max}"
            ));
        }
        if let Range::Parameter(p) = range
            && (p.clamp(mapping.min) != mapping.min || p.clamp(mapping.max) != mapping.max)
        {
            return Err("MIDI mapping choice endpoints must be whole values".into());
        }
        Ok(range)
    }

    pub(super) fn poll_midi_controls(&mut self) {
        let learned = self.io.bridge.controls.learned.swap(0, Ordering::Acquire);
        if learned != 0 {
            let packed = learned - 1;
            let channel = (packed >> 16) as u8;
            let controller = (packed >> 8) as u8;
            if self.io.bridge.controls.accepts(channel)
                && let MidiLearnStatus::Waiting { target } = self.io.controls.learn.clone()
            {
                self.io
                    .bridge
                    .controls
                    .learning
                    .store(false, Ordering::Release);
                let outcome = self.midi_target_range(&target).and_then(|range| {
                    let (min, max) = range.bounds();
                    let mapping = MidiMapping {
                        controller,
                        channel: Some(channel),
                        target,
                        min,
                        max,
                    };
                    self.add_midi_control_mapping(mapping.clone())?;
                    Ok(mapping)
                });
                self.io.controls.learn = match outcome {
                    Ok(mapping) => MidiLearnStatus::Learned { mapping },
                    Err(message) => MidiLearnStatus::Error { message },
                };
            }
        }

        // The array is fixed at 2048 entries. A dial can replace its pending
        // value repeatedly, but cannot grow work or memory between frames.
        let mut processed_control = false;
        let mut control_error = None;
        for channel in 0..CHANNELS as u8 {
            for controller in 0..CONTROLLERS as u8 {
                let index = usize::from(channel) * CONTROLLERS + usize::from(controller);
                let packed = self.io.bridge.controls.values[index].swap(0, Ordering::Acquire);
                if packed == 0 || !self.io.bridge.controls.accepts(channel) {
                    continue;
                }
                let has_specific = self
                    .io
                    .controls
                    .mappings
                    .iter()
                    .any(|m| m.controller == controller && m.channel == Some(channel));
                let mappings: Vec<_> = self
                    .io
                    .controls
                    .mappings
                    .iter()
                    .filter(|m| {
                        m.controller == controller
                            && (m.channel == Some(channel)
                                || (!has_specific && m.channel.is_none()))
                    })
                    .cloned()
                    .collect();
                if mappings.is_empty() {
                    // 割り当てのない CC は、選んでいる楽器にそのまま届ける
                    // （プラグインのモジュレーションホイールなど）。
                    let live = self
                        .io
                        .bridge
                        .live
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if let Some(live) = live.as_ref() {
                        live.expression(super::controller_event(controller, (packed - 1) as u8));
                    }
                    continue;
                }
                let now = Instant::now();
                if self
                    .io
                    .controls
                    .last_control
                    .is_none_or(|last| now.duration_since(last) >= GESTURE_IDLE)
                {
                    self.io.controls.gesture = self.io.controls.gesture.wrapping_add(1);
                }
                self.io.controls.last_control = Some(now);
                let gesture = self.io.controls.gesture;
                for mapping in mappings {
                    let range = match self.validate_midi_mapping(&mapping) {
                        Ok(range) => range,
                        Err(message) => {
                            control_error = Some(message);
                            continue;
                        }
                    };
                    processed_control = true;
                    let value = range.value(&mapping, (packed - 1) as u8);
                    let current = match &mapping.target {
                        MidiControlTarget::DeviceParam { device, key } => {
                            crate::find_device(self.project(), *device).and_then(|d| {
                                d.params.get(key).copied().or({
                                    if let Range::Parameter(p) = range {
                                        Some(p.default)
                                    } else {
                                        None
                                    }
                                })
                            })
                        }
                        MidiControlTarget::TrackGain { track } => {
                            self.project().mixer.track(*track).map(|t| t.gain_db)
                        }
                        MidiControlTarget::TrackPan { track } => {
                            self.project().mixer.track(*track).map(|t| t.pan)
                        }
                        MidiControlTarget::ChannelGain { channel } => {
                            self.project().channel(*channel).map(|c| c.gain_db)
                        }
                        MidiControlTarget::ChannelPan { channel } => {
                            self.project().channel(*channel).map(|c| c.pan)
                        }
                    };
                    if current == Some(value) {
                        continue;
                    }
                    match mapping.target {
                        MidiControlTarget::DeviceParam { device, key } => {
                            self.set_param(device, &key, value, gesture)
                        }
                        MidiControlTarget::TrackGain { track } => {
                            self.doc.edit_gesture(gesture, "MIDI track gain", |p| {
                                if let Some(t) = p.mixer.track_mut(track) {
                                    t.gain_db = value;
                                }
                            })
                        }
                        MidiControlTarget::TrackPan { track } => {
                            self.doc.edit_gesture(gesture, "MIDI track pan", |p| {
                                if let Some(t) = p.mixer.track_mut(track) {
                                    t.pan = value;
                                }
                            })
                        }
                        MidiControlTarget::ChannelGain { channel } => {
                            self.doc.edit_gesture(gesture, "MIDI channel gain", |p| {
                                if let Some(c) = p.channel_mut(channel) {
                                    c.gain_db = value;
                                }
                            })
                        }
                        MidiControlTarget::ChannelPan { channel } => {
                            self.doc.edit_gesture(gesture, "MIDI channel pan", |p| {
                                if let Some(c) = p.channel_mut(channel) {
                                    c.pan = value;
                                }
                            })
                        }
                    }
                }
            }
        }
        // A restored target clears its old warning on the next valid
        // controller message, while another failed mapping in the same
        // frame still reports its error.
        if processed_control || control_error.is_some() {
            self.io.controls.error = control_error;
        }
    }
}

#[cfg(test)]
mod tests;
