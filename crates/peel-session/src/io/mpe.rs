//! One MIDI Polyphonic Expression zone. All storage is fixed before input starts.
use super::{LiveInput, MidiEvent, Session};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MpeMode {
    #[default]
    Off,
    Lower,
    Upper,
}
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MpeConfig {
    pub mode: MpeMode,
    pub members: u8,
    pub member_bend_range: f32,
    pub manager_bend_range: f32,
}
impl Default for MpeConfig {
    fn default() -> Self {
        Self {
            mode: MpeMode::Off,
            members: 15,
            member_bend_range: 48.0,
            manager_bend_range: 2.0,
        }
    }
}
impl MpeConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=15).contains(&self.members) {
            return Err("MPE members must be between 1 and 15".into());
        }
        if !self.member_bend_range.is_finite()
            || !(0.0..=96.0).contains(&self.member_bend_range)
            || !self.manager_bend_range.is_finite()
            || !(0.0..=96.0).contains(&self.manager_bend_range)
        {
            return Err("MPE bend ranges must be between 0 and 96 semitones".into());
        }
        Ok(())
    }
    pub(super) fn manager(self) -> u8 {
        if self.mode == MpeMode::Upper { 15 } else { 0 }
    }
    fn member(self, ch: u8) -> bool {
        match self.mode {
            MpeMode::Off => false,
            MpeMode::Lower => ch > 0 && ch <= self.members,
            MpeMode::Upper => ch < 15 && ch >= 15 - self.members,
        }
    }
}
#[derive(Clone, Copy)]
struct Channel {
    bend: f32,
    pressure: f32,
    timbre: f32,
    rpn_msb: u8,
    rpn_lsb: u8,
    coarse: u8,
    fine: u8,
}
impl Default for Channel {
    fn default() -> Self {
        Self {
            bend: 0.0,
            pressure: 0.0,
            timbre: 0.5,
            rpn_msb: 127,
            rpn_lsb: 127,
            coarse: 0,
            fine: 0,
        }
    }
}
#[derive(Clone, Copy)]
struct Note {
    slot: u32,
    voice: u32,
    held: bool,
    sustained: bool,
    dirty: bool,
    release_due: bool,
    release_velocity: f32,
    poly_pressure: Option<f32>,
}
impl Default for Note {
    fn default() -> Self {
        Self {
            slot: peel_engine::LIVE_TARGET_NONE,
            voice: 0,
            held: false,
            sustained: false,
            dirty: false,
            release_due: false,
            release_velocity: 0.0,
            poly_pressure: None,
        }
    }
}
pub(super) struct MpeState {
    config: MpeConfig,
    listen_mode: MpeMode,
    channels: [Channel; 16],
    notes: [[Note; 128]; 16],
    pedal: bool,
    manager_dirty: bool,
    next_voice: u32,
}
impl Default for MpeState {
    fn default() -> Self {
        Self {
            config: MpeConfig::default(),
            listen_mode: MpeMode::Off,
            channels: [Channel::default(); 16],
            notes: [[Note::default(); 128]; 16],
            pedal: false,
            manager_dirty: false,
            next_voice: peel_engine::LIVE_VOICE_BASE + 128,
        }
    }
}
impl MpeState {
    pub(super) fn config(&self) -> MpeConfig {
        self.config
    }
    pub(super) fn reset(&mut self) {
        self.channels.fill(Channel::default());
        self.notes.fill([Note::default(); 128]);
        self.pedal = false;
        self.manager_dirty = false;
    }
    pub(super) fn configure(&mut self, config: MpeConfig) {
        self.reset();
        self.config = config;
        self.listen_mode = config.mode;
    }
    fn snapshot(&self, ch: u8) -> (f32, f32, f32) {
        let manager = self.channels[usize::from(self.config.manager())];
        let member = self.channels[usize::from(ch)];
        (
            (manager.bend * self.config.manager_bend_range
                + member.bend * self.config.member_bend_range)
                .clamp(-120.0, 120.0),
            (manager.pressure + member.pressure).clamp(0.0, 1.0),
            (manager.timbre + member.timbre - 0.5).clamp(0.0, 1.0),
        )
    }
    fn update(&mut self, live: &LiveInput, ch: u8) {
        let (pitch, pressure, timbre) = self.snapshot(ch);
        let manager_pressure = self.channels[usize::from(self.config.manager())].pressure;
        for key in 0..128 {
            let note = &mut self.notes[usize::from(ch)][key];
            if note.voice != 0 {
                let pressure = note
                    .poly_pressure
                    .map_or(pressure, |p| (manager_pressure + p).clamp(0.0, 1.0));
                note.dirty =
                    !live.voice_expression(note.slot, note.voice, ch, pitch, pressure, timbre);
            }
        }
    }
    fn update_all(&mut self, live: &LiveInput) {
        for ch in 0..16 {
            if self.config.member(ch) {
                self.update(live, ch);
            }
        }
    }
    fn send_manager(&mut self, live: &LiveInput) {
        let manager = self.channels[usize::from(self.config.manager())];
        self.manager_dirty = !live.expression_batch(&[
            peel_dsp::EventKind::PitchBendRange {
                semitones: self.config.manager_bend_range,
            },
            peel_dsp::EventKind::PitchBend {
                semitones: manager.bend * self.config.manager_bend_range,
            },
            peel_dsp::EventKind::ChannelPressure {
                value: manager.pressure,
            },
        ]);
    }
    pub(super) fn zone_change(&self, event: MidiEvent) -> bool {
        let manager = if self.listen_mode == MpeMode::Upper {
            15
        } else {
            0
        };
        matches!(event,MidiEvent::ControlChange{channel,controller:6,..} if self.listen_mode!=MpeMode::Off && channel==manager
            && self.channels[usize::from(channel)].rpn_msb==0 && self.channels[usize::from(channel)].rpn_lsb==6)
    }
    pub(super) fn flush(&mut self, live: &LiveInput) {
        // Releases precede expression retries and keep their critical reserve.
        for ch in 0..16u8 {
            for key in 0..128u8 {
                let note = &mut self.notes[usize::from(ch)][usize::from(key)];
                if note.release_due {
                    if !live.voice_off_with_velocity(
                        note.slot,
                        note.voice,
                        key,
                        ch,
                        note.release_velocity,
                    ) {
                        return;
                    }
                    note.release_due = false;
                    note.sustained = false;
                    note.held = false;
                }
            }
        }
        if self.manager_dirty {
            self.send_manager(live);
        }
        for ch in 0..16u8 {
            let (pitch, pressure, timbre) = self.snapshot(ch);
            let manager_pressure = self.channels[usize::from(self.config.manager())].pressure;
            for note in &mut self.notes[usize::from(ch)] {
                if note.dirty && note.voice != 0 {
                    let pressure = note
                        .poly_pressure
                        .map_or(pressure, |p| (manager_pressure + p).clamp(0.0, 1.0));
                    if !live.voice_expression(note.slot, note.voice, ch, pitch, pressure, timbre) {
                        return;
                    }
                    note.dirty = false;
                }
            }
        }
    }
    fn release(&mut self, live: &LiveInput, ch: u8, key: u8, velocity: f32) {
        let note = &mut self.notes[usize::from(ch)][usize::from(key)];
        if note.voice == 0 || !note.held {
            return;
        }
        note.release_velocity = velocity;
        if self.pedal {
            note.held = false;
            note.sustained = true;
        } else {
            note.held = false;
            note.release_due = !live.voice_off_with_velocity(
                note.slot,
                note.voice,
                key,
                ch,
                note.release_velocity,
            );
        }
    }
    fn pedal(&mut self, live: &LiveInput, down: bool) {
        live.sustain(down);
        // Plugins also get the pedal itself (see the non-MPE path).
        live.expression(peel_dsp::EventKind::Controller {
            cc: 64,
            value: if down { 1.0 } else { 0.0 },
        });
        self.pedal = down;
        if !down {
            for ch in 0..16u8 {
                for key in 0..128u8 {
                    let note = &mut self.notes[usize::from(ch)][usize::from(key)];
                    if note.sustained {
                        note.release_due = !live.voice_off_with_velocity(
                            note.slot,
                            note.voice,
                            key,
                            ch,
                            note.release_velocity,
                        );
                        if !note.release_due {
                            note.sustained = false;
                        }
                    }
                }
            }
        }
    }
    fn release_channel(&mut self, live: &LiveInput, ch: u8) {
        for key in 0..128u8 {
            let note = &mut self.notes[usize::from(ch)][usize::from(key)];
            if note.voice != 0
                && (note.held || note.sustained || note.release_due)
                && !live.voice_off_with_velocity(
                    note.slot,
                    note.voice,
                    key,
                    ch,
                    note.release_velocity,
                )
            {
                note.held = false;
                note.sustained = false;
                note.release_due = true;
            } else {
                *note = Note::default();
            }
        }
    }
    pub(super) fn manager_expression(
        &mut self,
        live: &LiveInput,
        kind: peel_dsp::EventKind,
    ) -> bool {
        if self.config.mode == MpeMode::Off {
            return false;
        }
        let manager = usize::from(self.config.manager());
        match kind {
            peel_dsp::EventKind::PitchBend { semitones } => {
                self.channels[manager].bend = semitones.clamp(-1.0, 1.0)
            }
            peel_dsp::EventKind::ChannelPressure { value } => {
                self.channels[manager].pressure = value.clamp(0.0, 1.0)
            }
            peel_dsp::EventKind::ResetExpression => {
                self.channels.fill(Channel::default());
                for channel in &mut self.notes {
                    for note in channel {
                        note.poly_pressure = None;
                    }
                }
            }
            peel_dsp::EventKind::PolyPressure { key, value } if key < 128 => {
                for channel in &mut self.notes {
                    channel[usize::from(key)].poly_pressure = Some(value.clamp(0.0, 1.0));
                }
            }
            _ => return false,
        }
        self.update_all(live);
        true
    }
    pub(super) fn manager_range(&mut self, live: &LiveInput, range: f32) {
        if self.config.mode != MpeMode::Off {
            self.config.manager_bend_range = range;
            self.update_all(live);
        }
    }

    pub(super) fn handle(&mut self, live: &LiveInput, event: MidiEvent) -> bool {
        if self.listen_mode == MpeMode::Off {
            return false;
        }
        let ch = event.channel();
        let listen_manager = if self.listen_mode == MpeMode::Upper {
            15
        } else {
            0
        };
        if self.config.mode == MpeMode::Off
            && !(ch == listen_manager
                && matches!(
                    event,
                    MidiEvent::ControlChange {
                        controller: 101 | 100 | 6 | 38,
                        ..
                    }
                ))
        {
            return false;
        }

        if ch > 15 {
            return true;
        }
        let manager = ch
            == if self.config.mode == MpeMode::Off {
                listen_manager
            } else {
                self.config.manager()
            };
        let member = self.config.member(ch);
        // MPE's configured zone replaces the single-channel note filter.
        if !manager && !member {
            return matches!(
                event,
                MidiEvent::NoteOn { .. }
                    | MidiEvent::NoteOff { .. }
                    | MidiEvent::PitchBend { .. }
                    | MidiEvent::ChannelPressure { .. }
                    | MidiEvent::PolyPressure { .. }
                    | MidiEvent::Sustain { .. }
                    | MidiEvent::AllNotesOff { .. }
                    | MidiEvent::AllSoundOff { .. }
            );
        }
        match event {
            MidiEvent::NoteOn { key, velocity, .. } if velocity > 0 && member && key < 128 => {
                let slot = live.target().get();
                let Some(slot) = slot else {
                    return true;
                };
                let previous = self.notes[usize::from(ch)][usize::from(key)];
                if previous.voice != 0
                    && (previous.held || previous.sustained || previous.release_due)
                {
                    let release_velocity = if previous.held {
                        0.0
                    } else {
                        previous.release_velocity
                    };
                    if !live.voice_off_with_velocity(
                        previous.slot,
                        previous.voice,
                        key,
                        ch,
                        release_velocity,
                    ) {
                        // Overload drops the new strike; only its old release retries.
                        let previous = &mut self.notes[usize::from(ch)][usize::from(key)];
                        previous.held = false;
                        previous.sustained = false;
                        previous.release_due = true;
                        previous.release_velocity = release_velocity;
                        return true;
                    }
                    self.notes[usize::from(ch)][usize::from(key)] = Note::default();
                }
                // A reallocated member channel stops controlling old release tails.
                for note in &mut self.notes[usize::from(ch)] {
                    if !note.held && !note.sustained && !note.release_due {
                        *note = Note::default();
                    }
                }
                let voice = self.next_voice;
                self.next_voice = self.next_voice.wrapping_add(1);
                if self.next_voice >= 0x7fff_ff00 {
                    self.next_voice = peel_engine::LIVE_VOICE_BASE + 128;
                }
                let (pitch, pressure, timbre) = self.snapshot(ch);
                if live.voice_on(
                    slot,
                    voice,
                    key,
                    f32::from(velocity) / 127.0,
                    ch,
                    pitch,
                    pressure,
                    timbre,
                ) {
                    self.notes[usize::from(ch)][usize::from(key)] = Note {
                        slot,
                        voice,
                        held: true,
                        sustained: false,
                        dirty: false,
                        release_due: false,
                        release_velocity: 0.0,
                        poly_pressure: None,
                    };
                }
                true
            }
            MidiEvent::NoteOff { key, velocity, .. } => {
                if member && key < 128 {
                    self.release(live, ch, key, f32::from(velocity.min(127)) / 127.0);
                }
                true
            }
            MidiEvent::NoteOn { key, .. } => {
                if member && key < 128 {
                    self.release(live, ch, key, 0.0);
                }
                true
            }
            MidiEvent::PitchBend { value, .. } => {
                self.channels[usize::from(ch)].bend =
                    f32::from(value) / if value < 0 { 8192.0 } else { 8191.0 };
                if manager {
                    self.send_manager(live);
                    self.update_all(live);
                } else {
                    self.update(live, ch);
                }
                true
            }
            MidiEvent::ChannelPressure { value, .. } => {
                self.channels[usize::from(ch)].pressure = f32::from(value.min(127)) / 127.0;
                if manager {
                    self.send_manager(live);
                    self.update_all(live);
                } else {
                    self.update(live, ch);
                }
                true
            }
            MidiEvent::PolyPressure { key, value, .. } => {
                if member && key < 128 {
                    self.notes[usize::from(ch)][usize::from(key)].poly_pressure =
                        Some(f32::from(value.min(127)) / 127.0);
                    self.update(live, ch);
                }
                true
            }
            MidiEvent::Sustain { down, .. } => {
                if manager {
                    self.pedal(live, down);
                }
                true
            }
            MidiEvent::AllNotesOff { .. } | MidiEvent::AllSoundOff { .. } => {
                if manager {
                    for c in 0..16 {
                        self.release_channel(live, c);
                    }
                    self.pedal = false;
                } else {
                    self.release_channel(live, ch);
                }
                true
            }
            MidiEvent::ControlChange {
                controller: 74,
                value,
                ..
            } => {
                self.channels[usize::from(ch)].timbre = if value <= 64 {
                    f32::from(value) / 128.0
                } else {
                    0.5 + f32::from(value.min(127) - 64) / 126.0
                };
                if manager {
                    self.update_all(live);
                } else {
                    self.update(live, ch);
                }
                true
            }
            MidiEvent::ControlChange {
                controller, value, ..
            } if matches!(controller, 101 | 100 | 6 | 38) => {
                match controller {
                    101 | 100 => {
                        let c = &mut self.channels[usize::from(ch)];
                        if controller == 101 {
                            c.rpn_msb = value;
                        } else {
                            c.rpn_lsb = value;
                        }
                        if c.rpn_msb == 0 && c.rpn_lsb == 0 {
                            let range = if manager {
                                self.config.manager_bend_range
                            } else {
                                self.config.member_bend_range
                            };
                            let cents = (range * 100.0).round() as u16;
                            c.coarse = (cents / 100) as u8;
                            c.fine = (cents % 100) as u8;
                        }
                    }
                    6 | 38 => {
                        let c = &mut self.channels[usize::from(ch)];
                        if controller == 6 {
                            c.coarse = value;
                        } else {
                            c.fine = value.min(99);
                        }
                        if c.rpn_msb == 0 && c.rpn_lsb == 0 {
                            let range =
                                (f32::from(c.coarse) + f32::from(c.fine) * 0.01).clamp(0.0, 96.0);
                            if manager {
                                self.config.manager_bend_range = range;
                                self.send_manager(live);
                            } else {
                                self.config.member_bend_range = range;
                            }
                            self.update_all(live);
                        } else if c.rpn_msb == 0 && c.rpn_lsb == 6 && controller == 6 && manager {
                            // A zero-size selected zone turns MPE off explicitly.
                            live.all_notes_off_target();
                            if value == 0 {
                                self.config.mode = MpeMode::Off;
                            } else {
                                self.config.mode = self.listen_mode;
                                self.config.members = value.min(15);
                            }
                            self.reset();
                        }
                    }
                    _ => {}
                }
                true
            }
            _ => false,
        }
    }
}
impl Session {
    pub fn configure_mpe(&mut self, config: MpeConfig) -> Result<(), String> {
        config.validate()?;
        if config.mode != MpeMode::Off && self.mpe_config() == config {
            return Ok(());
        }
        self.io.bridge.release_all();
        self.io
            .bridge
            .mpe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .configure(config);
        let guard = self
            .io
            .bridge
            .live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if config.mode != MpeMode::Off
            && let Some(live) = guard.as_ref()
        {
            live.expression(peel_dsp::EventKind::PitchBendRange {
                semitones: config.manager_bend_range,
            });
        }
        Ok(())
    }
    pub fn mpe_config(&self) -> MpeConfig {
        self.io
            .bridge
            .mpe
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use peel_engine::{EngineConfig, OfflineEngine, RecordedPerformanceKind};
    #[test]
    fn pending_poly_pressure_and_note_release_retry_after_queue_saturation() {
        let mut engine = OfflineEngine::new(
            48_000.0,
            &EngineConfig {
                device_slots: 1,
                max_block: 64,
                ..Default::default()
            },
        );
        let live = engine.take_live_input().unwrap();
        live.target().set(Some(0));
        let mut recorded = engine.take_recorded().unwrap();
        let mut state = MpeState::default();
        state.configure(MpeConfig {
            mode: MpeMode::Lower,
            members: 3,
            ..Default::default()
        });
        state.handle(
            &live,
            MidiEvent::NoteOn {
                channel: 1,
                key: 60,
                velocity: 100,
            },
        );
        let voice = state.notes[1][60].voice;
        while live.expression(peel_dsp::EventKind::ChannelPressure { value: 0.5 }) {}
        for _ in 0..128 {
            live.voice_off(0, peel_engine::LIVE_VOICE_BASE + 1000, 50, 1);
        }
        state.handle(
            &live,
            MidiEvent::PolyPressure {
                channel: 1,
                key: 60,
                value: 127,
            },
        );
        assert!(state.notes[1][60].dirty);
        state.handle(
            &live,
            MidiEvent::NoteOff {
                channel: 1,
                key: 60,
                velocity: 91,
            },
        );
        assert!(state.notes[1][60].release_due);
        let (mut l, mut r) = ([0.0; 64], [0.0; 64]);
        engine.render(&mut l, &mut r);
        state.flush(&live);
        assert!(!state.notes[1][60].release_due);
        assert!(!state.notes[1][60].dirty);
        let mut events = Vec::new();
        recorded.drain_performance_into(&mut events);
        assert!(
            events
                .iter()
                .any(|e| e.voice == voice
                    && matches!(e.kind, RecordedPerformanceKind::NoteOff {velocity,..} if (velocity-91.0/127.0).abs()<1e-7))
        );
        assert!(events.iter().any(|e| e.voice == voice
            && matches!(
                e.kind,
                RecordedPerformanceKind::Expression { pressure: 1.0, .. }
            )));
    }
    #[test]
    fn recording_overflow_is_bounded_and_reported_independently_from_legacy_notes() {
        let mut engine = OfflineEngine::new(
            48_000.0,
            &EngineConfig {
                device_slots: 1,
                max_block: 64,
                ..Default::default()
            },
        );
        let live = engine.take_live_input().unwrap();
        live.target().set(Some(0));
        let mut receiver = engine.take_recorded().unwrap();
        let (mut l, mut r) = ([0.0; 64], [0.0; 64]);
        for _ in 0..8 {
            for i in 0..800 {
                assert!(live.voice_expression(
                    0,
                    peel_engine::LIVE_VOICE_BASE + 200,
                    1,
                    (i % 48) as f32,
                    0.5,
                    0.5
                ));
            }
            for _ in 0..3 {
                engine.render(&mut l, &mut r);
            }
        }
        assert!(
            live.stats()
                .performance_dropped
                .load(std::sync::atomic::Ordering::Relaxed)
                > 0
        );
        assert_eq!(
            live.stats()
                .record_dropped
                .load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        let mut events = Vec::new();
        receiver.drain_performance_into(&mut events);
        assert_eq!(events.len(), peel_engine::live::RECORD_CAPACITY);
    }
}
