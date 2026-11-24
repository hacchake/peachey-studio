//! Shared plumbing for the native instruments.
//!
//! Every instrument implements [`Synth`], a small event-driven interface, and
//! is wrapped in [`Instrument`], which implements `peel_dsp::Device` once for
//! all of them: parameter storage and clamping, sample-accurate event
//! splitting, and finite-output protection.
//!
//! Block-size independence: instruments process sample by sample and keep
//! their control-rate counters across calls, so splitting the same timeline
//! into different block sizes gives the same output.

use std::sync::Arc;

use peel_dsp::filter::tanh_approx;
use peel_dsp::voice::{StealPolicy, VoiceAllocator, VoiceState};
use peel_dsp::{
    Device, DeviceDescriptor, DeviceEvent, EventKind, ParamId, ProcessContext, SampleBuffer,
};

/// Control-rate interval in samples. Filter coefficients, parameter smoothing
/// and other expensive per-voice updates run once per interval.
pub(crate) const CTRL: usize = 16;

/// Length of the anti-click fade applied to stolen or choked voices.
pub(crate) const STEAL_FADE_MS: f32 = 3.0;

/// Hard safety rail on instrument output; only pathological settings ever
/// reach it (it is +12 dBFS).
const OUTPUT_RAIL: f32 = 4.0;

/// The event-driven core of an instrument.
pub(crate) trait Synth: Send + 'static {
    /// Allocates voices and recomputes sample-rate dependent state. May
    /// allocate; the caller applies every parameter afterwards.
    fn prepare(&mut self, sample_rate: f32, max_block: usize);

    /// Silences everything without allocating.
    fn reset(&mut self);

    /// A parameter changed. `immediate` skips smoothing (initial setup and
    /// changes while the instrument is silent).
    fn param_changed(&mut self, id: ParamId, value: f32, immediate: bool);

    fn note_on(&mut self, key: u8, velocity: f32, id: u32);
    fn note_off(&mut self, key: u8, velocity: f32, id: u32);
    fn all_notes_off(&mut self);
    fn all_sound_off(&mut self);

    /// Overwrites `left`/`right` with the instrument output.
    fn render(&mut self, ctx: &ProcessContext, left: &mut [f32], right: &mut [f32]);

    fn is_silent(&self) -> bool;

    /// The synth's transient performance state. The synth ticks it from its
    /// own control interval and applies it to its voices.
    fn expression(&mut self) -> &mut Expression;

    fn set_sample(&mut self, sample: Option<Arc<SampleBuffer>>) -> Option<Arc<SampleBuffer>> {
        sample
    }
}

/// How many releases the sustain pedal can hold. Further releases while the
/// pedal is down are played at once instead (never allocates on the audio
/// thread).
const MAX_SUSTAINED: usize = 256;

/// MIDI controller number of the sustain pedal.
const SUSTAIN_CC: u8 = 64;

/// Adapts a [`Synth`] to the `Device` trait.
pub(crate) struct Instrument<S: Synth> {
    desc: &'static DeviceDescriptor,
    values: Vec<f32>,
    synth: S,
    /// The sustain pedal (CC64) is down.
    pedal: bool,
    /// Releases held by the pedal, in the order they were played:
    /// (key, release velocity, voice).
    sustained: Vec<(u8, f32, u32)>,
}

impl<S: Synth> Instrument<S> {
    pub(crate) fn new(desc: &'static DeviceDescriptor, synth: S) -> Instrument<S> {
        let mut inst = Instrument {
            desc,
            values: desc.params.iter().map(|p| p.default).collect(),
            synth,
            pedal: false,
            sustained: Vec::with_capacity(MAX_SUSTAINED),
        };
        // Usable even if the host never calls `prepare`.
        inst.prepare(48_000.0, 512);
        inst
    }

    fn apply(&mut self, id: ParamId, value: f32) {
        let Some(idx) = self.desc.params.iter().position(|p| p.id == id) else {
            return;
        };
        let v = self.desc.params[idx].clamp(value);
        self.values[idx] = v;
        // Changes while nothing sounds cannot be heard as zipper noise, so
        // snap to them; this also makes a preset fully applied before the
        // first note.
        let immediate = self.synth.is_silent();
        self.synth.param_changed(id, v, immediate);
    }

    fn handle(&mut self, kind: EventKind) {
        match kind {
            EventKind::NoteOn {
                key,
                velocity,
                voice,
            } => {
                let vel = if velocity.is_finite() {
                    velocity.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                // A new strike inherits channel pressure, not a previous
                // strike's per-key aftertouch (including its smoothing tail).
                // Striking a key the pedal holds ends the held note first,
                // as on a piano, so voices do not pile up.
                if let Some(i) = self.sustained.iter().position(|&(k, ..)| k == key) {
                    let (k, v, id) = self.sustained.remove(i);
                    self.release(k, v, id);
                }
                let ex = self.synth.expression();
                ex.clear_key(key);
                ex.claim(voice);
                self.synth.note_on(key.min(127), vel, voice);
            }
            EventKind::NoteOff {
                key,
                velocity,
                voice,
            } => {
                let vel = if velocity.is_finite() {
                    velocity.clamp(0.0, 1.0)
                } else {
                    0.0
                };
                if self.pedal && self.sustained.len() < MAX_SUSTAINED {
                    self.sustained.push((key, vel, voice));
                } else {
                    self.release(key, vel, voice);
                }
            }
            EventKind::Controller { cc, value } => {
                if cc == SUSTAIN_CC {
                    self.set_pedal(value >= 0.5);
                }
            }
            EventKind::Param { id, value } => self.apply(id, value),
            EventKind::PitchBend { semitones } => self.synth.expression().set_bend(semitones),
            // Bend events already carry actual semitones.
            EventKind::PitchBendRange { .. } => {}
            EventKind::ChannelPressure { value } => self.synth.expression().set_channel(value),
            EventKind::PolyPressure { key, value } => {
                self.synth.expression().set_poly(key, value);
            }
            EventKind::VoiceExpression {
                voice,
                pitch,
                pressure,
                timbre,
            } => self
                .synth
                .expression()
                .set_voice(voice, pitch, pressure, timbre),
            EventKind::ClearVoiceExpression { voice } => self.synth.expression().clear_voice(voice),
            // Live controllers return to centre; a sequenced note keeps the
            // curve it is playing.
            EventKind::ResetExpression => self.synth.expression().reset_shared(),
            EventKind::AllNotesOff => {
                self.pedal = false;
                self.sustained.clear();
                self.synth.all_notes_off();
            }
            EventKind::AllSoundOff => {
                self.pedal = false;
                self.sustained.clear();
                self.synth.expression().reset();
                self.synth.all_sound_off();
            }
        }
    }
}

impl<S: Synth> Instrument<S> {
    fn release(&mut self, key: u8, velocity: f32, voice: u32) {
        self.synth.expression().release(voice);
        self.synth.note_off(key.min(127), velocity, voice);
    }

    /// Pedal down holds releases; pedal up plays every held release.
    fn set_pedal(&mut self, down: bool) {
        self.pedal = down;
        if !down {
            // Index loop: `release` needs `&mut self` while we read the list.
            for i in 0..self.sustained.len() {
                let (k, v, id) = self.sustained[i];
                self.release(k, v, id);
            }
            self.sustained.clear();
        }
    }

    /// A note's snapshot may be planned right after its NoteOn at the same
    /// sample. Applying it first starts the note on its own pitch, pressure
    /// and timbre instead of gliding in from the shared state.
    fn prime(&mut self, events: &[DeviceEvent], at: usize) {
        let EventKind::NoteOn { voice, .. } = events[at].kind else {
            return;
        };
        let offset = events[at].offset;
        for e in events[at + 1..].iter().take_while(|e| e.offset == offset) {
            if let EventKind::VoiceExpression {
                voice: v,
                pitch,
                pressure,
                timbre,
            } = e.kind
                && v == voice
            {
                self.synth
                    .expression()
                    .prime(voice, pitch, pressure, timbre);
            }
        }
    }
}

impl<S: Synth> Device for Instrument<S> {
    fn descriptor(&self) -> &'static DeviceDescriptor {
        self.desc
    }

    fn prepare(&mut self, sample_rate: f32, max_block: usize) {
        let sr = if sample_rate.is_finite() {
            sample_rate.clamp(8_000.0, 384_000.0)
        } else {
            48_000.0
        };
        self.synth.prepare(sr, max_block.max(1));
        self.synth.expression().prepare(sr);
        for (p, &v) in self.desc.params.iter().zip(self.values.iter()) {
            self.synth.param_changed(p.id, v, true);
        }
    }

    fn reset(&mut self) {
        self.pedal = false;
        self.sustained.clear();
        self.synth.reset();
        self.synth.expression().reset();
    }

    fn set_param(&mut self, id: ParamId, value: f32) {
        self.apply(id, value);
    }

    fn param(&self, id: ParamId) -> f32 {
        self.desc
            .params
            .iter()
            .position(|p| p.id == id)
            .map_or(0.0, |i| self.values[i])
    }

    fn process(
        &mut self,
        ctx: &ProcessContext,
        events: &[DeviceEvent],
        left: &mut [f32],
        right: &mut [f32],
    ) {
        let frames = left.len().min(right.len());
        let mut pos = 0usize;
        let mut next_event = 0usize;
        let beats_per_sample = if ctx.sample_rate > 0.0 {
            ctx.tempo_bpm.clamp(1.0, 999.0) / 60.0 / ctx.sample_rate as f64
        } else {
            0.0
        };
        while pos < frames {
            while next_event < events.len() && (events[next_event].offset as usize) <= pos {
                self.prime(events, next_event);
                self.handle(events[next_event].kind);
                next_event += 1;
            }
            let end = match events.get(next_event) {
                Some(e) => (e.offset as usize).min(frames),
                None => frames,
            };
            let mut seg_ctx = *ctx;
            seg_ctx.beat_position = ctx.beat_position + pos as f64 * beats_per_sample;
            self.synth
                .render(&seg_ctx, &mut left[pos..end], &mut right[pos..end]);
            pos = end;
        }
        // Events at or past the end of the block (not expected) still apply so
        // note-offs are never lost.
        while next_event < events.len() {
            self.prime(events, next_event);
            self.handle(events[next_event].kind);
            next_event += 1;
        }
        for s in left[..frames].iter_mut().chain(right[..frames].iter_mut()) {
            *s = if s.is_finite() {
                s.clamp(-OUTPUT_RAIL, OUTPUT_RAIL)
            } else {
                0.0
            };
        }
        // Any unmatched tail of a longer buffer is silenced.
        for s in left[frames..].iter_mut().chain(right[frames..].iter_mut()) {
            *s = 0.0;
        }
    }

    fn is_silent(&self) -> bool {
        self.synth.is_silent()
    }

    fn set_sample(&mut self, sample: Option<Arc<SampleBuffer>>) -> Option<Arc<SampleBuffer>> {
        self.synth.set_sample(sample)
    }
}

/// A parameter value that approaches its target once per control interval.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ramp {
    cur: f32,
    tgt: f32,
}

impl Ramp {
    pub(crate) const fn new(v: f32) -> Ramp {
        Ramp { cur: v, tgt: v }
    }

    pub(crate) fn set(&mut self, v: f32, immediate: bool) {
        self.tgt = v;
        if immediate {
            self.cur = v;
        }
    }

    /// Moves towards the target; `k` comes from [`ramp_coef`].
    #[inline]
    pub(crate) fn tick(&mut self, k: f32) -> f32 {
        self.cur += (self.tgt - self.cur) * k;
        if (self.tgt - self.cur).abs() < 1.0e-6 * (1.0 + self.tgt.abs()) {
            self.cur = self.tgt;
        }
        self.cur
    }

    #[inline]
    pub(crate) fn get(&self) -> f32 {
        self.cur
    }
}

/// One-pole coefficient for [`Ramp::tick`] with a ~10 ms time constant at the
/// control rate.
pub(crate) fn ramp_coef(sample_rate: f32) -> f32 {
    1.0 - (-(CTRL as f32) / (0.010 * sample_rate)).exp()
}

pub(crate) use peel_dsp::voice::StealFade as Fade;

/// Behaviour the voice pool needs from a voice.
pub(crate) trait PoolVoice {
    fn is_active(&self) -> bool;
    /// Rough loudness used for quietest-voice stealing.
    fn level(&self) -> f32;
    fn fade_out(&mut self, sample_rate: f32);
    /// Stops immediately.
    fn kill(&mut self);
}

/// Preallocated voices plus the allocator that manages them.
pub(crate) struct Pool<V: PoolVoice> {
    pub voices: Vec<V>,
    pub alloc: VoiceAllocator,
    sample_rate: f32,
}

impl<V: PoolVoice> Pool<V> {
    pub(crate) fn new(
        capacity: usize,
        polyphony: usize,
        policy: StealPolicy,
        sample_rate: f32,
        make: impl Fn() -> V,
    ) -> Pool<V> {
        let alloc = VoiceAllocator::new(capacity, polyphony, policy);
        let voices = (0..alloc.capacity()).map(|_| make()).collect();
        Pool {
            voices,
            alloc,
            sample_rate,
        }
    }

    /// Releases bookkeeping for voices that have finished and refreshes the
    /// levels used for stealing.
    pub(crate) fn sync(&mut self) {
        for (i, v) in self.voices.iter().enumerate() {
            if v.is_active() {
                self.alloc.set_level(i, v.level());
            } else if self.alloc.slot(i).state != VoiceState::Free {
                self.alloc.free(i);
            }
        }
    }

    /// Picks the slot for a new note. The caller initializes `voices[slot]`.
    pub(crate) fn begin(&mut self, key: u8, id: u32) -> usize {
        self.sync();
        let a = self.alloc.allocate(key, id);
        if let Some(s) = a.stolen {
            self.voices[s].fade_out(self.sample_rate);
        }
        if let Some(c) = a.cut {
            self.voices[c].kill();
        }
        a.slot
    }

    /// Marks the voices started with `id` as releasing and calls `f` on each.
    pub(crate) fn release_id(&mut self, id: u32, mut f: impl FnMut(&mut V)) {
        let mut found = [0usize; 8];
        let n = self.alloc.find_held(id, &mut found);
        for &slot in &found[..n] {
            self.alloc.release(slot);
            f(&mut self.voices[slot]);
        }
    }

    /// Marks every held voice releasing and calls `f` on each.
    pub(crate) fn release_all(&mut self, mut f: impl FnMut(&mut V)) {
        for i in 0..self.voices.len() {
            if self.alloc.slot(i).state == VoiceState::Held {
                self.alloc.release(i);
                f(&mut self.voices[i]);
            }
        }
    }

    /// Calls `f` on each active voice with its key and runtime voice ID.
    pub(crate) fn for_active_id(&mut self, mut f: impl FnMut(&mut V, u8, u32)) {
        for (i, v) in self.voices.iter_mut().enumerate() {
            if v.is_active() {
                let s = self.alloc.slot(i);
                f(v, s.key, s.id);
            }
        }
    }

    pub(crate) fn kill_all(&mut self) {
        for v in &mut self.voices {
            v.kill();
        }
        self.alloc.reset();
    }

    pub(crate) fn is_silent(&self) -> bool {
        !self.voices.iter().any(PoolVoice::is_active)
    }
}

/// Velocity (0..1) to a gain that follows a gentle curve; `sens` blends from
/// fixed level (0) to fully velocity controlled (1).
#[inline]
pub(crate) fn vel_gain(velocity: f32, sens: f32) -> f32 {
    (1.0 - sens) + sens * velocity.powf(1.4)
}

/// Waveshaper settings for a 0..1 drive amount: `(gain, normalization)`.
#[inline]
pub(crate) fn drive_curve(drive: f32) -> (f32, f32) {
    let g = 1.0 + 8.0 * drive * drive;
    (g, 1.0 / tanh_approx(g))
}

/// Soft saturation that keeps a full-scale input at full scale.
#[inline]
pub(crate) fn shape(x: f32, gain: f32, norm: f32) -> f32 {
    tanh_approx(x * gain) * norm
}

#[inline]
pub(crate) fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Semitone offset of `key` from middle C (key 60), the drum transpose.
#[inline]
pub(crate) fn key_offset(key: u8) -> f32 {
    key as f32 - 60.0
}

/// Pitch ratio for a semitone offset.
#[inline]
pub(crate) fn st_ratio(st: f32) -> f32 {
    (st * (std::f32::consts::LN_2 / 12.0)).exp()
}

/// Largest pitch displacement, in semitones: the instrument-wide bend (an
/// MPE zone's manager channel reaches ±96) and a note's full snapshot
/// (manager plus member) alike.
pub(crate) const BEND_MAX: f32 = 120.0;
/// Gain added at full pressure: `1 + 0.6 * p` (about +4 dB).
pub(crate) const PRESSURE_GAIN: f32 = 0.6;
/// Filter cutoff opened at full pressure, in octaves.
pub(crate) const PRESSURE_OCTAVES: f32 = 0.75;
/// Cutoff moved by timbre (CC74) at either extreme, in octaves.
pub(crate) const TIMBRE_OCTAVES: f32 = 2.0;
/// Per-note expression entries kept at once. Larger than any voice pool, so
/// eviction only ever drops long-finished release tails.
const SCOPED: usize = 64;

/// The expression one voice should play with.
///
/// For a voice without its own snapshot these are exactly the legacy shared
/// values (instrument bend, channel and per-key pressure, neutral timbre).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Vx {
    /// Pitch displacement in semitones.
    pub bend: f32,
    /// `2^(bend/12)`, exactly 1.0 when centred.
    pub ratio: f32,
    /// Pressure 0..1.
    pub pressure: f32,
    /// Timbre as a bipolar amount -1..1; exactly 0.0 at the neutral 0.5.
    pub tone: f32,
}

impl Vx {
    /// Gain multiplier from pressure.
    #[inline]
    pub(crate) fn gain(&self) -> f32 {
        1.0 + PRESSURE_GAIN * self.pressure
    }

    /// Cutoff offset from pressure, in octaves.
    #[inline]
    pub(crate) fn octaves(&self) -> f32 {
        PRESSURE_OCTAVES * self.pressure
    }

    /// Cutoff offset from pressure and timbre together, in octaves.
    #[inline]
    pub(crate) fn cutoff_oct(&self) -> f32 {
        self.octaves() + TIMBRE_OCTAVES * self.tone
    }

    /// Multiplier for a brightness-like frequency: `2^(tone * octaves)`,
    /// exactly 1.0 at neutral timbre.
    #[inline]
    pub(crate) fn tone_ratio(&self, octaves: f32) -> f32 {
        if self.tone == 0.0 {
            1.0
        } else {
            (self.tone * octaves * std::f32::consts::LN_2).exp()
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Life {
    /// Not yet claimed by a NoteOn: a snapshot that arrived before its note,
    /// or one for a voice that was already sounding.
    Pending,
    Held,
    Released,
}

#[derive(Clone, Copy, Debug)]
struct Scoped {
    id: u32,
    life: Life,
    stamp: u32,
    pitch_t: f32,
    pitch: f32,
    ratio: f32,
    press_t: f32,
    press: f32,
    tone_t: f32,
    tone: f32,
}

impl Scoped {
    const EMPTY: Scoped = Scoped {
        id: 0,
        life: Life::Pending,
        stamp: 0,
        pitch_t: 0.0,
        pitch: 0.0,
        ratio: 1.0,
        press_t: 0.0,
        press: 0.0,
        tone_t: 0.0,
        tone: 0.0,
    };

    fn snap(&mut self) {
        self.pitch = self.pitch_t;
        self.press = self.press_t;
        self.tone = self.tone_t;
        self.ratio = if self.pitch == 0.0 {
            1.0
        } else {
            st_ratio(self.pitch)
        };
    }

    fn settled(&self) -> bool {
        self.pitch == self.pitch_t && self.press == self.press_t && self.tone == self.tone_t
    }
}

/// Transient pitch bend, aftertouch and timbre for one instrument.
///
/// Nothing here is saved and nothing retriggers a note. Values are smoothed
/// by [`Expression::tick`], which each synth calls from its own control
/// interval, so any block split gives the same output. At zero pressure and
/// zero bend every accessor returns exactly the neutral value (1.0 gain, 0.0
/// octaves, 0.0 semitones), so the baseline sound is unchanged.
///
/// Two layers:
/// - Shared (legacy MIDI): one bend, channel pressure and per-key pressure.
///   Channel pressure `c` and key pressure `k` combine as
///   `p = 1 - (1 - c)(1 - k)`.
/// - Per note: a snapshot keyed by runtime voice ID (MPE member channels and
///   recorded pattern curves). It replaces the shared values for that voice,
///   so a recorded take never adds a wheel's leftover position twice, and
///   two voices on the same key move independently.
///
/// Gain is `1 + 0.6 p`; filtered instruments also open the cutoff by
/// `0.75 p` octaves, and timbre moves it by up to two octaves either way.
pub(crate) struct Expression {
    bend_t: f32,
    bend: f32,
    ratio: f32,
    chan_t: f32,
    chan: f32,
    key_t: [f32; 128],
    key: [f32; 128],
    /// Keys whose smoothed value has not reached its target.
    moving: u128,
    /// Keys with a nonzero target or value.
    live: u128,
    k: f32,
    scoped: [Scoped; SCOPED],
    /// Entries in use, packed at the front of `scoped`.
    n: usize,
    /// Some entry has not reached its target.
    scoped_moving: bool,
    clock: u32,
}

impl Expression {
    pub(crate) const fn new() -> Expression {
        Expression {
            bend_t: 0.0,
            bend: 0.0,
            ratio: 1.0,
            chan_t: 0.0,
            chan: 0.0,
            key_t: [0.0; 128],
            key: [0.0; 128],
            moving: 0,
            live: 0,
            k: 0.5,
            scoped: [Scoped::EMPTY; SCOPED],
            n: 0,
            scoped_moving: false,
            clock: 0,
        }
    }

    /// Sets the smoothing for a sample rate (about 6 ms).
    pub(crate) fn prepare(&mut self, sample_rate: f32) {
        self.k = 1.0 - (-(CTRL as f32) / (0.006 * sample_rate)).exp();
    }

    pub(crate) fn set_bend(&mut self, semitones: f32) {
        self.bend_t = if semitones.is_finite() {
            semitones.clamp(-BEND_MAX, BEND_MAX)
        } else {
            0.0
        };
    }

    pub(crate) fn set_channel(&mut self, v: f32) {
        self.chan_t = unit(v);
    }

    pub(crate) fn set_poly(&mut self, key: u8, v: f32) {
        let i = key.min(127) as usize;
        self.key_t[i] = unit(v);
        self.moving |= 1u128 << i;
        self.live |= 1u128 << i;
    }

    pub(crate) fn clear_key(&mut self, key: u8) {
        let i = key.min(127) as usize;
        self.key_t[i] = 0.0;
        self.key[i] = 0.0;
        self.moving &= !(1u128 << i);
        self.live &= !(1u128 << i);
    }

    /// Back to centre and zero pressure at once, and every per-note
    /// snapshot ends (panic, device reset).
    pub(crate) fn reset(&mut self) {
        self.reset_shared();
        self.n = 0;
        self.scoped_moving = false;
    }

    /// The shared (legacy MIDI) bend and pressure snap to neutral. Per-note
    /// snapshots are untouched, so a recorded curve keeps playing.
    pub(crate) fn reset_shared(&mut self) {
        self.bend_t = 0.0;
        self.bend = 0.0;
        self.ratio = 1.0;
        self.chan_t = 0.0;
        self.chan = 0.0;
        self.key_t = [0.0; 128];
        self.key = [0.0; 128];
        self.moving = 0;
        self.live = 0;
    }

    fn find(&self, id: u32) -> Option<usize> {
        self.scoped[..self.n].iter().position(|e| e.id == id)
    }

    /// Index for a new entry, evicting the least useful one when full.
    fn slot_for(&mut self) -> usize {
        if self.n < SCOPED {
            self.n += 1;
            return self.n - 1;
        }
        let rank = |e: &Scoped| match e.life {
            Life::Released => 0u8,
            Life::Pending => 1,
            Life::Held => 2,
        };
        let mut best = 0;
        for i in 1..SCOPED {
            let (a, b) = (&self.scoped[i], &self.scoped[best]);
            let older = a.stamp.wrapping_sub(self.clock) < b.stamp.wrapping_sub(self.clock);
            if rank(a) < rank(b) || (rank(a) == rank(b) && older) {
                best = i;
            }
        }
        best
    }

    fn remove(&mut self, i: usize) {
        self.n -= 1;
        self.scoped[i] = self.scoped[self.n];
    }

    /// A cleared curve gives this sounding voice back to the shared controllers.
    pub(crate) fn clear_voice(&mut self, id: u32) {
        if let Some(i) = self.find(id) {
            self.remove(i);
        }
    }

    /// Sets the per-note snapshot of voice `id`. Pitch is the full note
    /// displacement in semitones, pressure and timbre are 0..1.
    pub(crate) fn set_voice(&mut self, id: u32, pitch: f32, pressure: f32, timbre: f32) {
        let i = match self.find(id) {
            Some(i) => i,
            None => {
                let i = self.slot_for();
                // A voice that is already sounding glides from where the
                // shared state had it rather than jumping.
                self.scoped[i] = Scoped {
                    id,
                    life: Life::Pending,
                    stamp: 0,
                    pitch_t: 0.0,
                    pitch: self.bend,
                    ratio: self.ratio,
                    press_t: 0.0,
                    press: self.chan,
                    tone_t: 0.0,
                    tone: 0.0,
                };
                i
            }
        };
        self.clock = self.clock.wrapping_add(1);
        let e = &mut self.scoped[i];
        e.stamp = self.clock;
        e.pitch_t = if pitch.is_finite() {
            pitch.clamp(-BEND_MAX, BEND_MAX)
        } else {
            0.0
        };
        e.press_t = unit(pressure);
        e.tone_t = if timbre.is_finite() {
            (timbre.clamp(0.0, 1.0) - 0.5) * 2.0
        } else {
            0.0
        };
        if e.life == Life::Released {
            // A later note reusing this ID announced itself early.
            e.life = Life::Pending;
        }
        if !e.settled() {
            self.scoped_moving = true;
        }
    }

    /// A snapshot planned for the note about to start as `id`.
    pub(crate) fn prime(&mut self, id: u32, pitch: f32, pressure: f32, timbre: f32) {
        if let Some(i) = self.find(id) {
            self.scoped[i].life = Life::Pending;
        }
        self.set_voice(id, pitch, pressure, timbre);
    }

    /// A note starts as `id`. A snapshot that arrived first becomes its
    /// starting state; one left by an earlier note with the same ID is stale.
    pub(crate) fn claim(&mut self, id: u32) {
        let Some(i) = self.find(id) else {
            return;
        };
        if self.scoped[i].life == Life::Pending {
            let e = &mut self.scoped[i];
            e.life = Life::Held;
            e.snap();
        } else {
            self.remove(i);
        }
    }

    /// The note started as `id` was released; its tail keeps its snapshot.
    pub(crate) fn release(&mut self, id: u32) {
        if let Some(i) = self.find(id) {
            self.scoped[i].life = Life::Released;
        }
    }

    /// Moves every value towards its target; call once per control interval.
    #[inline]
    pub(crate) fn tick(&mut self) {
        if self.bend != self.bend_t {
            self.bend = approach(self.bend, self.bend_t, self.k);
            self.ratio = if self.bend == 0.0 {
                1.0
            } else {
                st_ratio(self.bend)
            };
        }
        if self.chan != self.chan_t {
            self.chan = approach(self.chan, self.chan_t, self.k);
        }
        let mut m = self.moving;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            self.key[i] = approach(self.key[i], self.key_t[i], self.k);
            if self.key[i] == self.key_t[i] {
                self.moving &= !(1u128 << i);
                if self.key[i] == 0.0 {
                    self.live &= !(1u128 << i);
                }
            }
        }
        if self.scoped_moving {
            let mut any = false;
            for e in &mut self.scoped[..self.n] {
                if e.pitch != e.pitch_t {
                    e.pitch = approach(e.pitch, e.pitch_t, self.k);
                    e.ratio = if e.pitch == 0.0 {
                        1.0
                    } else {
                        st_ratio(e.pitch)
                    };
                }
                e.press = approach(e.press, e.press_t, self.k);
                e.tone = approach(e.tone, e.tone_t, self.k);
                any |= !e.settled();
            }
            self.scoped_moving = any;
        }
    }

    /// The expression for the voice started as `id` on `key`.
    #[inline]
    pub(crate) fn voice(&self, id: u32, key: u8) -> Vx {
        if self.n > 0
            && let Some(e) = self.scoped[..self.n].iter().find(|e| e.id == id)
        {
            return Vx {
                bend: e.pitch,
                ratio: e.ratio,
                pressure: e.press,
                tone: e.tone,
            };
        }
        Vx {
            bend: self.bend,
            ratio: self.ratio,
            pressure: self.pressure(key),
            tone: 0.0,
        }
    }

    /// Smoothed shared pitch displacement in semitones.
    #[inline]
    pub(crate) fn bend_st(&self) -> f32 {
        self.bend
    }

    /// Combined shared pressure 0..1 for a voice sounding `key`.
    #[inline]
    pub(crate) fn pressure(&self, key: u8) -> f32 {
        let k = if self.live == 0 {
            0.0
        } else {
            self.key[key.min(127) as usize]
        };
        self.chan + k - self.chan * k
    }
}

#[inline]
fn unit(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

#[inline]
fn approach(cur: f32, tgt: f32, k: f32) -> f32 {
    let next = cur + (tgt - cur) * k;
    if (tgt - next).abs() < 1.0e-6 * (1.0 + tgt.abs()) {
        tgt
    } else {
        next
    }
}

/// Limits an upward bend so `key + bend` stays below `max_hz`. Downward and
/// zero bends pass through, so the centred sound is never altered.
#[inline]
pub(crate) fn safe_bend(key: f32, bend: f32, max_hz: f32) -> f32 {
    if bend <= 0.0 {
        return bend;
    }
    let top = 69.0 + 12.0 * (max_hz / 440.0).log2();
    bend.min((top - key).max(0.0))
}

/// Per-voice brightness from timbre for voices without their own filter: a
/// first-order shelf pivoting at 1.2 kHz, up to 9 dB of treble cut or boost.
///
/// The low-pass state always runs, so engaging the shelf mid-note never
/// steps the output. At neutral timbre the input passes through bit-exact.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tilt {
    a: f32,
    z: [f32; 2],
    hi: f32,
}

impl Tilt {
    pub(crate) const fn new() -> Tilt {
        Tilt {
            a: 0.15,
            z: [0.0; 2],
            hi: 1.0,
        }
    }

    pub(crate) fn prepare(&mut self, sample_rate: f32) {
        self.a = 1.0 - (-std::f32::consts::TAU * 1_200.0 / sample_rate).exp();
    }

    pub(crate) fn reset(&mut self) {
        self.z = [0.0; 2];
        self.hi = 1.0;
    }

    /// Sets the shelf from a voice's timbre.
    #[inline]
    pub(crate) fn set(&mut self, x: &Vx) {
        self.hi = x.tone_ratio(1.5);
    }

    #[inline]
    pub(crate) fn tick_mono(&mut self, x: f32) -> f32 {
        self.z[0] += self.a * (x - self.z[0]);
        if self.hi == 1.0 {
            return x;
        }
        self.z[0] + self.hi * (x - self.z[0])
    }

    #[inline]
    pub(crate) fn tick(&mut self, l: f32, r: f32) -> (f32, f32) {
        self.z[0] += self.a * (l - self.z[0]);
        self.z[1] += self.a * (r - self.z[1]);
        if self.hi == 1.0 {
            return (l, r);
        }
        (
            self.z[0] + self.hi * (l - self.z[0]),
            self.z[1] + self.hi * (r - self.z[1]),
        )
    }
}
