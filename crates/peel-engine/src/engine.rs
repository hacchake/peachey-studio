//! The audio-thread engine: transport, sequencer and mixer graph.
//!
//! [`Engine`] is driven by [`Engine::process`] (any frame count) and is the
//! single implementation behind both the realtime callback and
//! [`OfflineEngine`](crate::OfflineEngine). Everything reachable from
//! `process` follows `docs/REALTIME.md`: no allocation, locks, I/O or
//! freeing; replaced objects leave through the retirement queue.
//!
//! # Timing model
//!
//! The engine counts output frames in `engine_sample` (monotonic `i64`).
//! While playing, an [`Origin`] (whole samples plus an exact fraction) is the
//! engine position at which musical time zero falls for the current run of
//! the transport. A beat `b` lands on engine sample
//! `origin + round(tempo.beat_to_seconds(b) * sample_rate)`, which equals
//! `TempoMap::beat_to_sample(b)` after a play from the start. The origin
//! changes only when the transport is (re)anchored: play, seek, plan swap,
//! and loop wrap (which adds the *unrounded* loop length). Nothing is
//! accumulated block by block and rounding never compounds, so repeating
//! material cannot drift and the result does not depend on how the host
//! splits the stream.
//!
//! # Event ordering
//!
//! Within a block, events are staged per device slot sorted by sample
//! offset. An event on a block's last frame belongs to that block; an event
//! exactly at `block_end` belongs to the next. At equal offsets a note-off is
//! delivered before note-ons (unless it ends a note that starts at that very
//! offset).

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use peel_dsp::{Device, EventKind, ProcessContext};
use rtrb::{Consumer, Producer};

use crate::audio::AudioState;
use crate::clock::{Origin, loop_samples};
use crate::command::{EngineCommand, Retired};
use crate::events::{ActiveNote, ActiveNotes, OffResult, Staging};
use crate::input::{InputInjector, InputShared, MonitorReceiver, new_processor};
use crate::live::{LiveInput, LiveShared, LiveState, LiveTiming, RecordedReceiver};
use crate::mixer::{Mixer, mix_ramped, peak, sanitize_peak, scale_ramped, slew, target_gains};
use crate::monitor::{Metronome, Preview};
use crate::plan::{AutomationTarget, RenderPlan, SeqKind};

/// MIDI controller number of the sustain pedal.
const SUSTAIN_CC: u8 = 64;
use crate::retire::Retirer;
use crate::telemetry::Telemetry;
use crate::{EngineConfig, LIVE_VOICE_BASE, MAX_TRACKS};

mod live_io;

/// Commands applied per block. The rest stay queued for the next block, which
/// bounds the time a burst (project load) can add to one callback.
const MAX_COMMANDS_PER_BLOCK: usize = 1024;
/// Gain/pan/mute ramp time. Constant in time, not in blocks.
const RAMP_SECONDS: f32 = 0.005;
/// Upper bound for linear gains accepted from commands (+30 dB).
const MAX_GAIN: f32 = 32.0;
/// Sequence events inspected on a plan swap when deciding whether a sounding
/// note will still receive its note-off.
const MAX_SWAP_SCAN: usize = 1 << 18;

/// Whether the transport runs and where.
struct Transport {
    playing: bool,
    /// Content position in quarter notes at `engine_sample`.
    position: f64,
    /// Engine sample at which musical time zero falls (while playing).
    origin: Origin,
    /// Engine sample of the last discontinuity (play, seek, loop wrap, plan
    /// swap). Audio regions already running at that point fade in.
    entry: i64,
    /// Index of the next sequence event to deliver.
    cursor: usize,
    /// The loop applies: the playhead was inside the loop when anchored.
    loop_armed: bool,
    /// The material end applies: the playhead started before it.
    end_armed: bool,
    /// Next whole beat to click.
    next_click: i64,
}

#[derive(Clone, Copy)]
struct BypassState {
    bypassed: bool,
    /// 1.0 = effect fully in the signal path, 0.0 = fully bypassed.
    mix: f32,
}

enum Boundary {
    None,
    /// Wrap from the loop end back to the loop start `(start, end)`.
    Loop(f64, f64),
    End,
}

/// The realtime engine. Create it on the control thread, move it to the audio
/// thread and call [`process`](Engine::process) from there.
pub struct Engine {
    sample_rate: f32,
    sr64: f64,
    max_block: usize,
    ramp_step: f32,

    commands: Option<Consumer<EngineCommand>>,
    retirer: Retirer,
    telemetry: Arc<Telemetry>,
    outputs: Box<crate::output::OutputTaps>,
    /// Live input queue, sustain pedal and callback timing.
    live: LiveState,

    slots: Vec<Option<Box<dyn Device>>>,
    slot_generations: Vec<u64>,
    bypass: Vec<BypassState>,
    suspended: Vec<bool>,
    restart_requests: Vec<Option<u64>>,
    stage: Staging,
    /// Per-slot "rendered in block `stamp`" marks, used to find orphans and
    /// to render a slot at most once per block.
    stamps: Vec<u32>,
    stamp: u32,
    active: ActiveNotes,
    /// Each articulation gets a fresh id so new curves leave old release tails alone.
    next_sequence_voice: u32,

    plan: Box<RenderPlan>,
    compensation: Box<crate::compensation::Compensation>,
    compensation_active: bool,
    device_revision: u64,
    compensated_device_revision: u64,
    transport: Transport,
    engine_sample: i64,

    mixer: Mixer,
    audio: AudioState,
    metronome: Metronome,
    preview: Preview,
    /// Audio input shared with the input callback and the control thread.
    input: Arc<InputShared>,
    /// Where monitored input arrives from the input callback, if connected.
    input_monitor: Option<Box<MonitorReceiver>>,
    /// Metronome, preview and input monitoring are audible (off for exports).
    monitoring: bool,

    measure_cpu: bool,
    timer: Option<Instant>,
    cpu_smooth: f32,
}

impl Engine {
    /// Builds an engine. Everything the audio thread will ever need is
    /// allocated here.
    ///
    /// * `commands`: the consumer end of the command queue, or `None` when
    ///   the owner calls [`handle_command`](Engine::handle_command) itself
    ///   (offline rendering).
    /// * `retire`: the producer end of the retirement queue.
    pub fn new(
        sample_rate: f32,
        config: &EngineConfig,
        telemetry: Arc<Telemetry>,
        retire: Producer<Retired>,
        commands: Option<Consumer<EngineCommand>>,
    ) -> Engine {
        let sample_rate = if sample_rate.is_finite() && sample_rate >= 8000.0 {
            sample_rate
        } else {
            48_000.0
        };
        let max_block = config.max_block.clamp(1, 16_384);
        let device_slots = config.device_slots.clamp(1, 16_384);
        telemetry
            .sample_rate
            .store(sample_rate.round() as u32, Ordering::Relaxed);
        Engine {
            sample_rate,
            sr64: sample_rate as f64,
            max_block,
            ramp_step: 1.0 / (RAMP_SECONDS * sample_rate).max(1.0),
            commands,
            retirer: Retirer::new(retire, telemetry.clone()),
            live: LiveState::new(telemetry.clone(), sample_rate),
            telemetry,
            slots: (0..device_slots).map(|_| None).collect(),
            slot_generations: vec![0; device_slots],
            suspended: vec![false; device_slots],
            restart_requests: vec![None; device_slots],
            bypass: vec![
                BypassState {
                    bypassed: false,
                    mix: 1.0
                };
                device_slots
            ],
            stage: Staging::new(device_slots, config.events_per_block),
            stamps: vec![0; device_slots],
            stamp: 0,
            active: ActiveNotes::new(),
            next_sequence_voice: 1,
            plan: Box::new(RenderPlan::empty()),
            compensation: Box::new(
                crate::compensation::Compensation::prepare(&RenderPlan::empty(), &[], max_block)
                    .expect("empty compensation graph"),
            ),
            compensation_active: false,
            device_revision: 0,
            compensated_device_revision: 0,
            transport: Transport {
                playing: false,
                position: 0.0,
                origin: Origin::ZERO,
                entry: 0,
                cursor: 0,
                loop_armed: false,
                end_armed: false,
                next_click: 0,
            },
            engine_sample: 0,
            outputs: Box::default(),
            mixer: Mixer::new(max_block, device_slots),
            audio: AudioState::new(sample_rate),
            metronome: Metronome::new(sample_rate),
            preview: Preview::new(sample_rate),
            input: Arc::new(InputShared::new(sample_rate)),
            input_monitor: None,
            monitoring: true,
            measure_cpu: true,
            timer: None,
            cpu_smooth: 0.0,
        }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn max_block(&self) -> usize {
        self.max_block
    }

    pub fn is_playing(&self) -> bool {
        self.transport.playing
    }

    /// Takes the producer end of the live input queue (once). Move it to the
    /// MIDI thread; see [`LiveInput`].
    pub fn take_live_input(&mut self) -> Option<LiveInput> {
        self.live.take_input()
    }

    /// Takes the consumer end of the ring of recorded live notes (once).
    pub fn take_recorded(&mut self) -> Option<RecordedReceiver> {
        self.live.take_record()
    }

    /// Chooses how live events are placed inside a block.
    pub fn set_live_timing(&mut self, timing: LiveTiming) {
        self.live.shared().set_timing(timing);
    }

    pub(crate) fn live_shared(&self) -> &std::sync::Arc<LiveShared> {
        self.live.shared()
    }

    /// The audio input state this engine shares with an input stream: meter,
    /// capture, monitoring and timing. See [`crate::input`].
    pub fn input_shared(&self) -> &Arc<InputShared> {
        &self.input
    }

    /// Connects an input that no device feeds and returns the injector that
    /// feeds it, for engines driven directly (tests, tools). With a device
    /// engine use [`EngineHandle::open_injected_input`](crate::EngineHandle::open_injected_input).
    /// Monitoring resamples this input to the engine's output clock.
    pub fn open_injected_input(
        &mut self,
        telemetry: &Arc<Telemetry>,
        sample_rate: u32,
        channels: u16,
    ) -> InputInjector {
        let out_rate = self.sample_rate.round() as u32;
        let (processor, parts) = new_processor(
            &self.input,
            telemetry,
            sample_rate,
            channels.max(1),
            out_rate,
        );
        self.input.attach(
            parts.sample_rate,
            parts.channels,
            parts.capture,
            parts.monitor.is_some(),
        );
        self.handle_command(EngineCommand::SetInputMonitor(parts.monitor.map(Box::new)));
        InputInjector::new(processor)
    }

    /// Tells the input side how the output callback is timed, so a take can
    /// be placed against what the player hears (see [`crate::latency`]).
    /// `callback_ns` and `lead_ns` are zero when the host reports no timing.
    /// Realtime-safe.
    #[inline]
    pub fn note_output_timing(&self, callback_ns: u64, lead_ns: u64, frames: usize) {
        let graph_ns =
            (self.compensation_latency_samples() as f64 / self.sr64 * 1e9).round() as u64;
        let lead_ns = if graph_ns > 0 && lead_ns == 0 {
            (frames as f64 / self.sr64 * 1e9).round() as u64
        } else {
            lead_ns
        };
        self.input
            .note_output_timing(callback_ns, lead_ns.saturating_add(graph_ns), frames);
    }

    pub fn compensation_latency_samples(&self) -> u32 {
        if self.compensation_active {
            self.compensation.latency_samples()
        } else {
            0
        }
    }

    pub fn slot_latency(&self, slot: usize) -> u32 {
        self.slots
            .get(slot)
            .and_then(Option::as_ref)
            .map_or(0, |d| d.latency())
    }

    /// Control-side snapshot used when preparing an offline render.
    pub fn slot_latencies(&self) -> Vec<u32> {
        self.slots
            .iter()
            .map(|d| d.as_ref().map_or(0, |d| d.latency()))
            .collect()
    }

    /// Content position in quarter notes at the next sample to be rendered.
    pub fn position_beats(&self) -> f64 {
        self.transport.position
    }

    /// Frames rendered since the engine was created.
    pub fn engine_sample(&self) -> i64 {
        self.engine_sample
    }

    /// Revision of the active plan.
    pub fn plan_revision(&self) -> u64 {
        self.plan.revision
    }

    /// Enables or disables the metronome and preview voice in the output.
    /// Realtime engines monitor; offline exports do not.
    pub fn set_monitoring(&mut self, on: bool) {
        self.monitoring = on;
        if !on {
            self.metronome.clear();
        }
    }

    /// Enables CPU load and overrun measurement in [`process`](Engine::process).
    /// Offline renders turn it off: wall-clock time says nothing there.
    pub fn set_cpu_metering(&mut self, on: bool) {
        self.measure_cpu = on;
    }

    // ------------------------------------------------------------------
    // Processing entry points
    // ------------------------------------------------------------------

    /// Renders `left.len().min(right.len())` frames, splitting into
    /// `max_block` chunks, and updates the CPU load telemetry.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let frames = left.len().min(right.len());
        self.begin_callback_frames(frames);
        self.render(left, right);
        self.end_callback(frames);
    }

    /// Starts timing a device callback. Pair with [`end_callback`](Engine::end_callback)
    /// when one callback is rendered through several [`render`](Engine::render)
    /// calls.
    #[inline]
    pub fn begin_callback(&mut self) {
        self.begin_callback_frames(0);
    }

    /// [`begin_callback`](Engine::begin_callback) for a callback that will
    /// render `frames` frames in total (0 if unknown). Knowing the size lets
    /// live events be placed in the right chunk of a long callback.
    #[inline]
    pub fn begin_callback_frames(&mut self, frames: usize) {
        if self.measure_cpu {
            self.timer = Some(Instant::now());
        }
        let now = self.live.now_ns();
        self.live.begin_callback(now, frames);
    }

    /// Finishes timing a callback that produced `frames` frames: updates the
    /// smoothed CPU load and counts a missed deadline as an overrun.
    pub fn end_callback(&mut self, frames: usize) {
        if !self.measure_cpu || frames == 0 {
            return;
        }
        let Some(t0) = self.timer.take() else {
            return;
        };
        let spent = t0.elapsed().as_secs_f64();
        let deadline = frames as f64 / self.sr64;
        let load = (spent / deadline) as f32;
        if load > 1.0 {
            self.telemetry.overruns.fetch_add(1, Ordering::Relaxed);
        }
        // Fast attack so spikes show; slow release (~0.3 s) so the number is
        // readable.
        let alpha = ((deadline / 0.3) as f32).min(1.0);
        if load > self.cpu_smooth {
            self.cpu_smooth = load;
        } else {
            self.cpu_smooth += (load - self.cpu_smooth) * alpha;
        }
        self.telemetry.set_cpu_load(self.cpu_smooth);
    }

    /// Renders audio without touching the CPU meter. Any frame count is
    /// accepted; longer requests run as `max_block` chunks, so the output
    /// does not depend on the request size.
    pub fn render(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        for x in &mut left[n..] {
            *x = 0.0;
        }
        for x in &mut right[n..] {
            *x = 0.0;
        }
        let mut done = 0;
        while done < n {
            let frames = (n - done).min(self.max_block);
            self.render_chunk(
                &mut left[done..done + frames],
                &mut right[done..done + frames],
            );
            done += frames;
        }
    }

    // ------------------------------------------------------------------
    // Commands
    // ------------------------------------------------------------------

    fn drain_commands(&mut self) {
        let Some(mut rx) = self.commands.take() else {
            return;
        };
        let mut n = 0;
        while n < MAX_COMMANDS_PER_BLOCK {
            match rx.pop() {
                Ok(cmd) => {
                    self.handle_command(cmd);
                    n += 1;
                }
                Err(_) => break,
            }
        }
        self.commands = Some(rx);
    }

    /// Applies one command immediately. The realtime path calls this at the
    /// start of each block; offline renders call it between `render` calls.
    pub fn handle_command(&mut self, cmd: EngineCommand) {
        match cmd {
            EngineCommand::SetPlan(plan) => {
                if self.set_plan(plan) {
                    self.compensation_active = false;
                    self.compensation.reset();
                    self.telemetry
                        .compensation_latency
                        .store(0, Ordering::Relaxed);
                }
            }
            EngineCommand::SetCompensatedPlan { plan, compensation } => {
                if self.set_plan(plan) {
                    self.set_compensation(compensation);
                } else {
                    self.retirer.retire(Retired::Compensation(compensation));
                }
            }
            EngineCommand::SetCompensation(compensation) => self.set_compensation(compensation),
            EngineCommand::SetOutputRoutes(routes) => {
                let old = std::mem::replace(&mut self.outputs, routes);
                self.retirer.retire(Retired::OutputRoutes(old));
            }
            EngineCommand::InstallCompensatedDevice {
                slot,
                device,
                generation,
            } => {
                if let Some(dest) = self.slot_generations.get_mut(slot as usize) {
                    *dest = generation;
                }
                self.handle_command(EngineCommand::InstallDevice { slot, device });
            }
            EngineCommand::RemoveCompensatedDevice { slot, generation } => {
                if let Some(dest) = self.slot_generations.get_mut(slot as usize) {
                    *dest = generation;
                }
                self.handle_command(EngineCommand::RemoveDevice { slot });
            }
            EngineCommand::InstallDevice { slot, device } => {
                if self.telemetry.analysis.is_selected(slot) {
                    self.telemetry.analysis.invalidate();
                }
                let s = slot as usize;
                if s >= self.slots.len() {
                    self.retirer.retire(Retired::Device(device));
                    return;
                }
                self.device_revision = self.device_revision.wrapping_add(1);
                self.suspended[s] = false;
                self.restart_requests[s] = None;
                if let Some(old) = self.slots[s].replace(device) {
                    self.retirer.retire(Retired::Device(old));
                }
                self.active.remove_slot(slot);
            }
            EngineCommand::SuspendDevice { slot, token } => {
                if let Some(request) = self.restart_requests.get_mut(slot as usize) {
                    *request = Some(token);
                    self.transfer_restarts();
                }
            }
            EngineCommand::RemoveDevice { slot } => {
                if self.telemetry.analysis.is_selected(slot) {
                    self.telemetry.analysis.invalidate();
                }
                let s = slot as usize;
                if let Some(suspended) = self.suspended.get_mut(s) {
                    *suspended = false;
                    self.restart_requests[s] = None;
                }
                if let Some(old) = self.slots.get_mut(s).and_then(Option::take) {
                    self.device_revision = self.device_revision.wrapping_add(1);
                    self.retirer.retire(Retired::Device(old));
                }
                self.active.remove_slot(slot);
            }
            EngineCommand::SetParam { slot, id, value } => {
                if let Some(Some(dev)) = self.slots.get_mut(slot as usize) {
                    dev.set_param(id, value);
                }
            }
            EngineCommand::SetSample { slot, sample } => match self.slots.get_mut(slot as usize) {
                Some(Some(dev)) => {
                    if let Some(old) = dev.set_sample(sample) {
                        self.retirer.retire(Retired::Sample(old));
                    }
                }
                _ => {
                    if let Some(s) = sample {
                        self.retirer.retire(Retired::Sample(s));
                    }
                }
            },
            EngineCommand::SetBypass { slot, bypass } => {
                if let Some(b) = self.bypass.get_mut(slot as usize) {
                    if b.bypassed != bypass && self.telemetry.analysis.is_selected(slot) {
                        self.telemetry.analysis.invalidate();
                    }
                    b.bypassed = bypass;
                }
            }
            EngineCommand::SetChannelGain { channel, gain } => {
                if let Some(c) = self.plan.channels.get_mut(channel as usize)
                    && gain.is_finite()
                {
                    c.gain = gain.clamp(0.0, MAX_GAIN);
                }
            }
            EngineCommand::SetChannelPan { channel, pan } => {
                if let Some(c) = self.plan.channels.get_mut(channel as usize)
                    && pan.is_finite()
                {
                    c.pan = pan.clamp(-1.0, 1.0);
                }
            }
            EngineCommand::SetChannelMute { channel, muted } => {
                if let Some(c) = self.plan.channels.get_mut(channel as usize) {
                    c.muted = muted;
                }
            }
            EngineCommand::SetTrackGain { track, gain } => {
                if let Some(t) = self.plan.tracks.get_mut(track as usize)
                    && gain.is_finite()
                {
                    t.gain = gain.clamp(0.0, MAX_GAIN);
                }
            }
            EngineCommand::SetTrackPan { track, pan } => {
                if let Some(t) = self.plan.tracks.get_mut(track as usize)
                    && pan.is_finite()
                {
                    t.pan = pan.clamp(-1.0, 1.0);
                }
            }
            EngineCommand::SetSendGain { track, send, gain } => {
                if let Some(t) = self.plan.tracks.get_mut(track as usize)
                    && let Some(sd) = t.sends.get_mut(send as usize)
                    && gain.is_finite()
                {
                    sd.gain = gain.clamp(0.0, MAX_GAIN);
                }
            }
            EngineCommand::SetTrackMute { track, muted } => {
                if let Some(t) = self.plan.tracks.get_mut(track as usize) {
                    t.muted = muted;
                }
            }
            EngineCommand::Play => {
                if !self.transport.playing {
                    self.transport.playing = true;
                    let pos = self.start_beat(self.transport.position, true);
                    self.transport.position = pos;
                    self.anchor(pos);
                    self.transport.next_click = ceil_beat(pos);
                }
            }
            EngineCommand::Pause => {
                self.compensation.reset();
                self.telemetry.analysis.invalidate();
                self.release_sequence_notes(0);
                if self.transport.playing {
                    self.cut_audio(self.engine_sample);
                }
                self.transport.playing = false;
            }
            EngineCommand::Stop { return_to } => {
                self.compensation.reset();
                self.telemetry.analysis.invalidate();
                self.release_sequence_notes(0);
                if self.transport.playing {
                    self.cut_audio(self.engine_sample);
                }
                self.transport.playing = false;
                self.transport.position = sane_beat(return_to);
            }
            EngineCommand::Seek { beat } => {
                self.compensation.reset();
                self.telemetry.analysis.invalidate();
                self.release_sequence_notes(0);
                if self.transport.playing {
                    self.cut_audio(self.engine_sample);
                }
                let beat = sane_beat(beat);
                self.transport.position = beat;
                if self.transport.playing {
                    self.anchor(beat);
                    self.transport.next_click = ceil_beat(beat);
                }
            }
            EngineCommand::Metronome { enabled } => {
                self.metronome.enabled = enabled;
            }
            EngineCommand::LiveNote {
                slot,
                key,
                velocity,
                on,
            } => self.live_note(slot, key, velocity, on, 0),
            EngineCommand::Preview { sample, gain } => {
                if self.monitoring {
                    self.preview.start(sample, gain, &mut self.retirer);
                } else if let Some(s) = sample {
                    self.retirer.retire(Retired::Sample(s));
                }
            }
            EngineCommand::Panic => self.panic(),
            EngineCommand::SetInputMonitor(monitor) => {
                if let Some(old) = std::mem::replace(&mut self.input_monitor, monitor) {
                    self.retirer.retire(Retired::Monitor(old));
                }
            }
        }
    }

    /// Starts or releases a live note `offset` frames into the block.
    fn live_note(&mut self, slot: u32, key: u8, velocity: f32, on: bool, offset: u32) {
        let s = slot as usize;
        let key = key.min(127);
        let voice = LIVE_VOICE_BASE + key as u32;
        if on {
            if !matches!(self.slots.get(s), Some(Some(_))) {
                return;
            }
            // A repeated key retriggers: release the old note first.
            if self.active.remove_live(slot, key).is_some() {
                stage_off(
                    &mut self.stage,
                    &mut self.active,
                    &self.telemetry,
                    s,
                    offset,
                    EventKind::NoteOff {
                        key,
                        velocity: 0.0,
                        voice,
                    },
                );
            }
            let ev = EventKind::NoteOn {
                key,
                velocity: clean_velocity(velocity),
                voice,
            };
            if self.active.is_full() || !self.stage.push_on(s, offset, ev) {
                self.telemetry.dropped_notes.fetch_add(1, Ordering::Relaxed);
                return;
            }
            self.active.push(ActiveNote {
                slot,
                key,
                voice,
                source_voice: 0,
                live: true,
                keep: true,
            });
        } else if self.active.remove_live(slot, key).is_some() {
            stage_off(
                &mut self.stage,
                &mut self.active,
                &self.telemetry,
                s,
                offset,
                EventKind::NoteOff {
                    key,
                    velocity: clean_velocity(velocity),
                    voice,
                },
            );
        }
    }

    fn panic(&mut self) {
        self.compensation.reset();
        self.telemetry.analysis.invalidate();
        self.active.clear();
        self.live.reset();
        for s in 0..self.slots.len() {
            if self.slots[s].is_some() {
                self.stage.push_control(s, 0, EventKind::AllSoundOff);
            }
        }
        self.metronome.clear();
        if self.preview.is_active() {
            self.preview.start(None, 1.0, &mut self.retirer);
        }
    }

    // ------------------------------------------------------------------
    // Plan swap
    // ------------------------------------------------------------------

    fn set_plan(&mut self, new: Box<RenderPlan>) -> bool {
        let valid = !new.tracks.is_empty()
            && new.tracks.len() <= MAX_TRACKS
            && new.channels.len() <= self.slots.len()
            && (new.master as usize) < new.tracks.len();
        if !valid {
            self.telemetry
                .plans_rejected
                .fetch_add(1, Ordering::Relaxed);
            self.retirer.retire(Retired::Plan(new));
            return false;
        }
        if self.transport.playing {
            // Regions of the old plan fade out as the new plan's fade in, so
            // a region that survives the swap is unchanged and one that does
            // not leaves without a click.
            self.cut_audio(self.engine_sample);
        }
        let old_channels = self.plan.channels.len();
        let old_tracks = self.plan.tracks.len();
        self.remap_source_identities(&new);
        let old = std::mem::replace(&mut self.plan, new);
        // Indices that disappear forget their smoothing state, so a later
        // plan reusing them starts at its own gain instead of ramping from
        // an unrelated one.
        for r in self
            .mixer
            .chan_ramp
            .iter_mut()
            .take(old_channels)
            .skip(self.plan.channels.len())
        {
            r.init = false;
        }
        for r in self
            .mixer
            .track_ramp
            .iter_mut()
            .take(old_tracks)
            .skip(self.plan.tracks.len())
        {
            r.init = false;
        }
        self.telemetry
            .plan_revision
            .store(self.plan.revision, Ordering::Relaxed);
        if self.transport.playing {
            // A loop that now ends behind the playhead (a shorter or another
            // pattern) is rejoined at its start.
            let pos = self.start_beat(self.transport.position, false);
            self.transport.position = pos;
            self.anchor(pos);
            self.reconcile_notes();
        }
        self.retirer.retire(Retired::Plan(old));
        true
    }

    /// Match a still-sounding note by musical identity before event-order ids
    /// change. The two prepared indexes keep this O(active voices * log notes).
    fn remap_source_identities(&mut self, new: &RenderPlan) {
        let Engine {
            plan: old,
            active,
            stage,
            live,
            ..
        } = self;
        if old.note_identities.is_empty() && new.note_identities.is_empty() {
            return;
        }
        for note in active.as_mut_slice().iter_mut().filter(|n| !n.live) {
            let old_source = note.source_voice;
            note.source_voice = old
                .note_identities
                .binary_search_by_key(&old_source, |n| n.voice)
                .ok()
                .and_then(|i| {
                    let identity = old.note_identities[i].identity;
                    new.note_identity_index
                        .binary_search_by_key(&identity, |(id, _)| *id)
                        .ok()
                        .map(|i| new.note_identity_index[i].1)
                })
                .unwrap_or(crate::LIVE_TARGET_NONE);
            let expressed = |plan: &RenderPlan, voice| {
                plan.expression_notes
                    .binary_search_by_key(&voice, |n| n.voice)
                    .ok()
                    .is_some_and(|i| !plan.expression_notes[i].points.is_empty())
            };
            if note.source_voice != crate::LIVE_TARGET_NONE
                && expressed(old, old_source)
                && !expressed(new, note.source_voice)
            {
                let kind = EventKind::ClearVoiceExpression { voice: note.voice };
                if stage.push_on(note.slot as usize, 0, kind) {
                    live.clear_deferred_expression(note.slot, kind);
                } else {
                    live.defer_expression(note.slot, kind);
                }
            }
        }
    }

    fn transfer_restarts(&mut self) {
        for s in 0..self.restart_requests.len() {
            let Some(token) = self.restart_requests[s] else {
                continue;
            };
            // Keep processing the existing instance until retirement capacity
            // becomes available. An ownership transfer must never be leaked.
            if !self.retirer.can_accept() {
                break;
            }
            self.restart_requests[s] = None;
            if let Some(mut device) = self.slots[s].take() {
                device.reset();
                self.suspended[s] = true;
                self.active.remove_slot(s as u32);
                self.live.forget_slot(s as u32);
                self.retirer.retire(Retired::RestartDevice {
                    slot: s as u32,
                    token,
                    device,
                });
            }
        }
    }

    fn set_compensation(&mut self, mut compensation: Box<crate::compensation::Compensation>) {
        if self.compensation_active && self.compensated_device_revision == self.device_revision {
            compensation.inherit_history(&mut self.compensation);
        }
        self.telemetry
            .compensation_latency
            .store(compensation.latency_samples(), Ordering::Relaxed);
        let old = std::mem::replace(&mut self.compensation, compensation);
        self.compensation_active = true;
        self.compensated_device_revision = self.device_revision;
        self.retirer.retire(Retired::Compensation(old));
    }

    /// After a plan swap: notes whose note-off still lies ahead in the new
    /// plan keep sounding; every other sequence note is released now, so a
    /// note can never hang because its note-off disappeared with the old plan.
    fn reconcile_notes(&mut self) {
        let sequence_notes = self
            .active
            .as_mut_slice()
            .iter()
            .filter(|n| !n.live)
            .count();
        if sequence_notes == 0 {
            return;
        }
        for n in self.active.as_mut_slice() {
            n.keep = n.live;
        }
        self.active
            .as_mut_slice()
            .sort_unstable_by_key(|n| n.source_voice);
        let mut remaining = sequence_notes;
        {
            let plan = &*self.plan;
            let notes = self.active.as_mut_slice();
            let ahead = plan.sequence.get(self.transport.cursor..).unwrap_or(&[]);
            for ev in ahead.iter().take(MAX_SWAP_SCAN) {
                let SeqKind::NoteOff { key, voice, .. } = ev.kind else {
                    continue;
                };
                let slot = plan.channels.get(ev.channel as usize).map(|c| c.slot);
                // Source ids identify realized notes across plan updates.
                // Check the slot and key before retaining the actual DSP voice.
                let start = notes.partition_point(|n| n.source_voice < voice);
                for n in notes[start..]
                    .iter_mut()
                    .take_while(|n| n.source_voice == voice)
                {
                    if !n.keep && n.key == key && slot == Some(n.slot) {
                        n.keep = true;
                        remaining -= 1;
                        break;
                    }
                }
                if remaining == 0 {
                    break;
                }
            }
        }
        let mut i = 0;
        while i < self.active.len() {
            if self.active.get(i).keep {
                i += 1;
            } else {
                let n = self.active.swap_remove(i);
                self.stage_note_off(n, 0);
            }
        }
    }

    // ------------------------------------------------------------------
    // Transport
    // ------------------------------------------------------------------

    /// Where playback from `beat` begins. Past the end of the loop it is the
    /// loop start, since the wrap is only taken from inside the loop; with
    /// `restart`, a plan that has already played to its end starts again.
    fn start_beat(&self, beat: f64, restart: bool) -> f64 {
        match valid_loop(&self.plan, self.sr64) {
            Some((start, end)) if beat >= end => start,
            None if restart && beat >= self.plan.length_beats => 0.0,
            _ => beat,
        }
    }

    /// Anchors the running transport so that `beat` is the current engine
    /// sample, and positions the event cursor and boundary flags.
    fn anchor(&mut self, beat: f64) {
        let sr = self.sr64;
        let plan = &*self.plan;
        let t = &mut self.transport;
        t.origin = Origin::anchored(&plan.tempo, beat, self.engine_sample, sr);
        t.entry = self.engine_sample;
        t.cursor = cursor_at(plan, &t.origin, self.engine_sample, sr);
        t.loop_armed = valid_loop(plan, sr).is_some_and(|(_, end)| beat < end);
        t.end_armed = plan.length_beats.is_finite() && beat < plan.length_beats;
        self.chase_expression_notes(beat, 0);
    }

    /// Chases only notes with recorded expression. Their prepared intervals
    /// avoid searching note-on/off pairs or allocating during a transport jump.
    fn chase_expression_notes(&mut self, beat: f64, offset: u32) {
        let Engine {
            plan,
            stage,
            live,
            active,
            slots,
            telemetry,
            next_sequence_voice,
            ..
        } = self;
        for (matched, note) in plan.sounding_expression(beat).enumerate() {
            if matched >= crate::MAX_ACTIVE_NOTES {
                telemetry.dropped_notes.fetch_add(1, Ordering::Relaxed);
                break;
            }
            let Some(route) = plan.channels.get(note.channel as usize) else {
                continue;
            };
            let slot = route.slot as usize;
            if !slots.get(slot).is_some_and(Option::is_some) {
                continue;
            }
            let existing = active.remove_voice(route.slot, note.voice);
            let mut actual_voice = note.voice;
            let sounding = if let Some(existing) = existing {
                if existing.key == note.key {
                    actual_voice = existing.voice;
                    active.push(existing);
                    true
                } else {
                    stage_off(
                        stage,
                        active,
                        telemetry,
                        slot,
                        offset,
                        EventKind::NoteOff {
                            key: existing.key,
                            velocity: 0.0,
                            voice: existing.voice,
                        },
                    );
                    false
                }
            } else {
                false
            };
            if !sounding {
                if active.is_full() || !stage.has_on_room(slot, 2) {
                    telemetry.dropped_notes.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                actual_voice = sequence_voice(next_sequence_voice);
                stage.push_on(
                    slot,
                    offset,
                    EventKind::NoteOn {
                        key: note.key,
                        velocity: clean_velocity(note.velocity),
                        voice: actual_voice,
                    },
                );
                active.push(ActiveNote {
                    slot: route.slot,
                    key: note.key,
                    voice: actual_voice,
                    source_voice: note.voice,
                    live: false,
                    keep: false,
                });
            }
            let end = note.points.partition_point(|p| p.beat <= beat);
            if let Some(state) = end.checked_sub(1).and_then(|i| note.points.get(i)) {
                let kind = EventKind::VoiceExpression {
                    voice: actual_voice,
                    pitch: state.pitch,
                    pressure: state.pressure,
                    timbre: state.timbre,
                };
                if stage.push_on(slot, offset, kind) {
                    live.clear_deferred_expression(route.slot, kind);
                } else {
                    live.defer_expression(route.slot, kind);
                }
            }
        }
    }

    /// The transport is about to jump or stop at engine sample `at`: let the
    /// audio regions sounding there fade out instead of ending abruptly.
    fn cut_audio(&mut self, at: i64) {
        self.audio.cut(
            &self.plan,
            &self.transport.origin,
            self.transport.entry,
            at,
            self.sr64,
            &self.telemetry,
        );
    }

    /// Stages the release of a tracked note. If the slot has no room left,
    /// `Staging::finish_chunk` falls back to `AllNotesOff`.
    fn stage_note_off(&mut self, n: ActiveNote, offset: u32) {
        stage_off(
            &mut self.stage,
            &mut self.active,
            &self.telemetry,
            n.slot as usize,
            offset,
            EventKind::NoteOff {
                key: n.key,
                velocity: 0.0,
                voice: n.voice,
            },
        );
    }

    /// Releases every sequence-driven note `offset` frames into the block.
    /// Live notes (keyboard, MIDI in) are left alone.
    fn release_sequence_notes(&mut self, offset: u32) {
        // A pedal the arrangement holds down is lifted too, and controller
        // lanes send their value again when playback resumes.
        for lane in self.plan.automation.iter_mut() {
            if let AutomationTarget::Controller { slot, cc } = lane.target {
                if cc == SUSTAIN_CC && lane.last >= 0.5 && lane.last.is_finite() {
                    self.stage.push_control(
                        slot as usize,
                        offset,
                        EventKind::Controller { cc, value: 0.0 },
                    );
                }
                lane.last = crate::plan::AUTOMATION_UNSET;
            }
        }
        let mut i = 0;
        while i < self.active.len() {
            if self.active.get(i).live {
                i += 1;
            } else {
                let n = self.active.swap_remove(i);
                self.stage_note_off(n, offset);
            }
        }
    }

    // ------------------------------------------------------------------
    // Block rendering
    // ------------------------------------------------------------------

    fn render_chunk(&mut self, left: &mut [f32], right: &mut [f32]) {
        let frames = left.len();
        self.retirer.flush();
        self.drain_commands();
        self.transfer_restarts();
        // Plugins publish latency changes through their cached, realtime-safe
        // Device report. The control thread prepares replacement storage.
        for (slot, dev) in self
            .slots
            .iter()
            .enumerate()
            .take(self.telemetry.slot_latencies.len())
        {
            let latency = dev.as_ref().map_or(0, |d| d.latency());
            let changed =
                self.telemetry.slot_latencies[slot].swap(latency, Ordering::Relaxed) != latency;
            let generation = self.slot_generations[slot];
            let acknowledged = self.telemetry.slot_generations[slot]
                .swap(generation, Ordering::Release)
                != generation;
            if changed || acknowledged {
                self.telemetry
                    .latency_revision
                    .fetch_add(1, Ordering::Release);
            }
        }
        self.drain_live(frames);

        let block_start = self.engine_sample;
        let block_end = block_start + frames as i64;
        let beat_start = self.transport.position;
        let was_playing = self.transport.playing;

        self.audio.begin_chunk();
        if was_playing {
            self.schedule(block_start, block_end);
            self.apply_automation(beat_start, block_start, block_end);
        }
        let overflows = self.stage.finish_chunk(&mut self.active);
        if overflows > 0 {
            self.telemetry
                .note_overflows
                .fetch_add(overflows as u64, Ordering::Relaxed);
        }

        self.mix(frames, beat_start, was_playing, left, right);
        self.finish_chunk(block_end);
    }

    /// Applies every automation lane active at `beat`. Devices and the mixer
    /// smooth the change, so once per chunk is enough. MIDI controller lanes
    /// are events instead: a pedal must go down and up on its exact sample,
    /// so their breakpoints inside the block are delivered at their offsets.
    fn apply_automation(&mut self, beat: f64, block_start: i64, block_end: i64) {
        let plan = &mut *self.plan;
        let sr = self.sr64;
        let origin = self.transport.origin;
        // After a loop wrap inside this block the origin no longer matches
        // `beat`; controller breakpoints then wait for the next block.
        let aligned = (origin.beat_at(&plan.tempo, block_start, sr) - beat).abs() < 1e-6;
        for lane in plan.automation.iter_mut() {
            if let AutomationTarget::Controller { slot, cc } = lane.target {
                let mut send = |lane: &mut crate::plan::AutomationLane, offset: u32, v: f32| {
                    if v.is_finite() && (v - lane.last).abs() > f32::EPSILON {
                        lane.last = v;
                        self.stage.push_control(
                            slot as usize,
                            offset,
                            EventKind::Controller {
                                cc,
                                value: v.clamp(0.0, 1.0),
                            },
                        );
                    }
                };
                if let Some(v) = lane.value_at(beat) {
                    send(lane, 0, v);
                }
                if aligned {
                    let frames = (block_end - block_start).max(0) as u32;
                    for i in 0..lane.points.len() {
                        let at = lane.points[i].0;
                        if at <= beat {
                            continue;
                        }
                        let sample = origin.sample_of(&plan.tempo, at, sr);
                        if sample >= block_end {
                            break;
                        }
                        let offset = (sample - block_start).clamp(0, i64::from(frames) - 1) as u32;
                        if let Some(v) = lane.value_at(at) {
                            send(lane, offset, v);
                        }
                    }
                }
                continue;
            }
            let Some(v) = lane.value_at(beat) else {
                continue;
            };
            if !v.is_finite() || (v - lane.last).abs() <= f32::EPSILON {
                continue;
            }
            lane.last = v;
            match lane.target {
                AutomationTarget::Param { slot, id } => {
                    if let Some(Some(dev)) = self.slots.get_mut(slot as usize) {
                        dev.set_param(id, v);
                    }
                }
                AutomationTarget::TrackGain(t) => {
                    if let Some(tr) = plan.tracks.get_mut(t as usize) {
                        tr.gain = v.clamp(0.0, MAX_GAIN);
                    }
                }
                AutomationTarget::TrackPan(t) => {
                    if let Some(tr) = plan.tracks.get_mut(t as usize) {
                        tr.pan = v.clamp(-1.0, 1.0);
                    }
                }
                AutomationTarget::Controller { .. } => {}
            }
        }
    }

    /// Delivers sequence events and metronome clicks for
    /// `[block_start, block_end)`, splitting at loop and sequence ends.
    fn schedule(&mut self, block_start: i64, block_end: i64) {
        let sr = self.sr64;
        let frames = (block_end - block_start) as usize;
        let mut seg_start = block_start;
        // Each wrap consumes at least one sample, so this bound is never hit
        // by valid plans; it only guards against pathological ones.
        for _ in 0..=frames + 1 {
            let mut seg_end = block_end;
            let mut boundary = Boundary::None;

            let looping = if self.transport.loop_armed {
                valid_loop(&self.plan, sr)
            } else {
                None
            };
            if let Some((start, end)) = looping {
                let end_s = self.transport.origin.sample_of(&self.plan.tempo, end, sr);
                if end_s < block_end {
                    seg_end = end_s.max(seg_start);
                    boundary = Boundary::Loop(start, end);
                }
            } else if self.transport.loop_armed {
                // The loop was removed or became invalid.
                self.transport.loop_armed = false;
            }
            if matches!(boundary, Boundary::None) && self.transport.end_armed {
                let end_s =
                    self.transport
                        .origin
                        .sample_of(&self.plan.tempo, self.plan.length_beats, sr);
                if end_s < block_end {
                    seg_end = end_s.max(seg_start);
                    boundary = Boundary::End;
                }
            }

            self.emit_events(block_start, seg_end);
            self.emit_clicks(block_start, seg_end);
            self.audio.push_segment(
                seg_start,
                seg_end,
                self.transport.origin,
                self.transport.entry,
            );

            match boundary {
                Boundary::None => break,
                Boundary::Loop(start, end) => {
                    let offset = (seg_end - block_start) as u32;
                    self.release_sequence_notes(offset);
                    self.cut_audio(seg_end);
                    let plan = &*self.plan;
                    let t = &mut self.transport;
                    t.entry = seg_end;
                    t.origin.wrap(&plan.tempo, start, end, sr);
                    t.cursor = cursor_at(plan, &t.origin, seg_end, sr);
                    t.next_click = ceil_beat(start);
                    self.chase_expression_notes(start, offset);
                    seg_start = seg_end;
                }
                Boundary::End => {
                    let offset = (seg_end - block_start) as u32;
                    self.release_sequence_notes(offset);
                    self.cut_audio(seg_end);
                    self.transport.playing = false;
                    self.transport.position = self.plan.length_beats;
                    break;
                }
            }
        }
    }

    /// Stages sequence events whose engine sample is below `until`.
    fn emit_events(&mut self, block_start: i64, until: i64) {
        let sr = self.sr64;
        let Engine {
            plan,
            transport,
            stage,
            live,
            active,
            slots,
            telemetry,
            next_sequence_voice,
            ..
        } = self;
        let seq = &plan.sequence;
        while transport.cursor < seq.len() {
            let ev = &seq[transport.cursor];
            let at = transport.origin.sample_of(&plan.tempo, ev.beat, sr);
            if at >= until {
                break;
            }
            transport.cursor += 1;
            // `at < until <= block end`, so the offset is inside the block.
            let offset = (at - block_start).max(0) as u32;
            let Some(route) = plan.channels.get(ev.channel as usize) else {
                continue;
            };
            let slot = route.slot as usize;
            match ev.kind {
                SeqKind::NoteOn {
                    key,
                    velocity,
                    voice,
                } => {
                    if !matches!(slots.get(slot), Some(Some(_))) {
                        continue;
                    }
                    let span = plan
                        .expression_notes
                        .binary_search_by_key(&voice, |n| n.voice)
                        .ok()
                        .map(|i| &plan.expression_notes[i]);
                    let seed = span.and_then(|n| {
                        let end = n.points.partition_point(|p| p.beat <= ev.beat);
                        end.checked_sub(1).map(|i| n.points[i])
                    });
                    // A retained note at this onset still needs its revised seed,
                    // but it must not retrigger its envelope.
                    if let Some(actual_voice) = active.voice_for_source(route.slot, voice) {
                        if let Some(seed) = seed {
                            let kind = EventKind::VoiceExpression {
                                voice: actual_voice,
                                pitch: seed.pitch,
                                pressure: seed.pressure,
                                timbre: seed.timbre,
                            };
                            if stage.push_on(slot, offset, kind) {
                                live.clear_deferred_expression(route.slot, kind);
                            } else {
                                live.defer_expression(route.slot, kind);
                            }
                        }
                        continue;
                    }
                    if voice >= LIVE_VOICE_BASE
                        || active.is_full()
                        || !stage.has_on_room(slot, 1 + usize::from(seed.is_some()))
                    {
                        telemetry.dropped_notes.fetch_add(1, Ordering::Relaxed);
                        continue;
                    }
                    let actual_voice = sequence_voice(next_sequence_voice);
                    stage.push_on(
                        slot,
                        offset,
                        EventKind::NoteOn {
                            key,
                            velocity: clean_velocity(velocity),
                            voice: actual_voice,
                        },
                    );
                    if let Some(seed) = seed {
                        // Staged together, so a chord cannot consume the seed's
                        // room with other NoteOns and start on the wrong pitch.
                        let kind = EventKind::VoiceExpression {
                            voice: actual_voice,
                            pitch: seed.pitch,
                            pressure: seed.pressure,
                            timbre: seed.timbre,
                        };
                        stage.push_on(slot, offset, kind);
                        live.clear_deferred_expression(route.slot, kind);
                    }
                    active.push(ActiveNote {
                        slot: route.slot,
                        key,
                        voice: actual_voice,
                        source_voice: voice,
                        live: false,
                        keep: false,
                    });
                }
                SeqKind::Expression {
                    voice,
                    pitch,
                    pressure,
                    timbre,
                } => {
                    // NoteOn already staged this seed atomically. Chased notes
                    // also receive their initial state as a pair.
                    let initial = plan
                        .expression_notes
                        .binary_search_by_key(&voice, |n| n.voice)
                        .ok()
                        .is_some_and(|i| {
                            plan.expression_notes[i]
                                .points
                                .first()
                                .is_some_and(|p| p.beat == ev.beat)
                        });
                    if initial {
                        continue;
                    }
                    if let Some(actual_voice) = active.voice_for_source(route.slot, voice) {
                        let kind = EventKind::VoiceExpression {
                            voice: actual_voice,
                            pitch,
                            pressure,
                            timbre,
                        };
                        if stage.push_on(slot, offset, kind) {
                            live.clear_deferred_expression(route.slot, kind);
                        } else {
                            live.defer_expression(route.slot, kind);
                        }
                    }
                }
                SeqKind::NoteOff {
                    key,
                    velocity,
                    voice,
                } => {
                    // Only notes the engine actually started are released.
                    if let Some(note) = active.remove_voice(route.slot, voice) {
                        stage_off(
                            stage,
                            active,
                            telemetry,
                            slot,
                            offset,
                            EventKind::NoteOff {
                                key,
                                velocity: clean_velocity(velocity),
                                voice: note.voice,
                            },
                        );
                    }
                }
            }
        }
    }

    /// Starts metronome clicks for whole beats below `until`.
    fn emit_clicks(&mut self, block_start: i64, until: i64) {
        if !self.monitoring {
            return;
        }
        let sr = self.sr64;
        let per_bar = self.plan.beats_per_bar.max(1) as i64;
        // A click is at most a few hundred beats per block even at the
        // fastest tempo and the largest block, but bound it regardless.
        for _ in 0..4096 {
            let n = self.transport.next_click;
            let at = self
                .transport
                .origin
                .sample_of(&self.plan.tempo, n as f64, sr);
            if at >= until {
                break;
            }
            self.transport.next_click = n + 1;
            if self.metronome.enabled {
                let offset = (at - block_start).max(0) as u32;
                self.metronome.trigger(n % per_bar == 0, offset);
            }
        }
    }

    fn finish_chunk(&mut self, block_end: i64) {
        // Slots that received events but are not rendered by the plan
        // (orphans) cannot act on them; release by resetting the device.
        for i in 0..self.stage.touched_len() {
            let slot = self.stage.touched_slot(i);
            if self.stamps[slot] == self.stamp {
                continue;
            }
            let releases = self.stage.events(slot).iter().any(|e| {
                matches!(
                    e.kind,
                    EventKind::NoteOff { .. } | EventKind::AllNotesOff | EventKind::AllSoundOff
                )
            });
            if releases && let Some(dev) = self.slots[slot].as_mut() {
                dev.reset();
            }
        }
        self.stage.clear();

        self.engine_sample = block_end;
        if self.transport.playing {
            let beat = self
                .transport
                .origin
                .beat_at(&self.plan.tempo, block_end, self.sr64);
            self.transport.position = if beat.is_finite() {
                beat.max(0.0)
            } else {
                self.transport.position
            };
        }
        self.telemetry.set_beat(self.transport.position);
        self.telemetry
            .playing
            .store(self.transport.playing, Ordering::Relaxed);
    }

    // ------------------------------------------------------------------
    // Mixer
    // ------------------------------------------------------------------

    fn mix(
        &mut self,
        frames: usize,
        beat_start: f64,
        playing: bool,
        out_l: &mut [f32],
        out_r: &mut [f32],
    ) {
        self.stamp = self.stamp.wrapping_add(1);
        if self.stamp == 0 {
            self.stamp = 1;
        }
        let step = self.ramp_step;
        let block_start = self.engine_sample;
        let Engine {
            plan,
            slots,
            bypass,
            suspended,
            stage,
            stamps,
            stamp,
            mixer,
            compensation,
            compensation_active,
            outputs,
            telemetry,
            sample_rate,
            audio,
            retirer,
            sr64,
            ..
        } = self;
        let ctx = ProcessContext {
            sample_rate: *sample_rate,
            tempo_bpm: plan.tempo.bpm_at(beat_start),
            beat_position: beat_start,
            playing,
        };
        let n_tracks = plan.tracks.len();
        mixer.dirty[..n_tracks].fill(false);
        mixer.key_ready[..n_tracks].fill(false);

        // ---- audio regions and declick tails -> tracks ----
        audio.render(plan, mixer, retirer, block_start, frames, *sr64);
        if *compensation_active {
            for ti in 0..n_tracks {
                if !mixer.dirty[ti] && !compensation.audio[ti].has_pending() {
                    continue;
                }
                mixer.ensure_clean(ti, frames);
                let (l, r) = mixer.track(ti, frames);
                compensation.audio[ti].process(l, r);
                // Delayed material can remain audible after the source ends.
                mixer.dirty[ti] = peak(l) > 0.0 || peak(r) > 0.0;
            }
        }

        // ---- instrument channels -> tracks ----
        for (ci, ch) in plan.channels.iter().enumerate() {
            let slot = ch.slot as usize;
            let track = ch.track as usize;
            if slot >= slots.len() || track >= n_tracks || stamps[slot] == *stamp {
                continue;
            }
            stamps[slot] = *stamp;
            let Some(dev) = slots[slot].as_mut() else {
                continue;
            };
            let (tl, tr) = target_gains(ch.gain, ch.pan, ch.muted);
            let events = stage.events(slot);
            let silent = events.is_empty() && dev.is_silent();
            if silent && (!*compensation_active || !compensation.channels[ci].has_pending()) {
                mixer.chan_ramp[ci].snap(tl, tr);
                continue;
            }
            let mut ramp = mixer.chan_ramp[ci];
            let was_dirty = mixer.dirty[track];
            let mut wrote = false;
            {
                let (sl, sr, dl, dr) = mixer.scratch_and_track(track, frames);
                if silent {
                    sl.fill(0.0);
                    sr.fill(0.0);
                } else {
                    dev.process(&ctx, events, sl, sr);
                }
                if *compensation_active {
                    compensation.channels[ci].process(sl, sr);
                }
                let (pl, pr, bad) = sanitize_peak(sl, sr);
                if bad > 0 {
                    telemetry
                        .non_finite
                        .fetch_add(bad as u64, Ordering::Relaxed);
                    dev.reset();
                }
                let level = pl.max(pr);
                if level > 0.0 {
                    telemetry.push_slot(ch.slot, level);
                }
                if !ramp.is_silent(tl, tr) {
                    if was_dirty {
                        mix_ramped::<true>(dl, dr, sl, sr, &mut ramp, tl, tr, step);
                    } else {
                        mix_ramped::<false>(dl, dr, sl, sr, &mut ramp, tl, tr, step);
                    }
                    wrote = level > 0.0;
                }
            }
            mixer.chan_ramp[ci] = ramp;
            if wrote {
                mixer.dirty[track] = true;
            }
        }

        // ---- tracks in plan order ----
        let master = plan.master as usize;
        for ti in 0..n_tracks {
            let route = &plan.tracks[ti];

            for (ii, &insert) in route.inserts.iter().enumerate() {
                let s = insert as usize;
                if s >= slots.len() {
                    continue;
                }
                let Some(dev) = slots[s].as_mut() else {
                    if suspended[s] {
                        mixer.ensure_clean(ti, frames);
                        let (l, r) = mixer.track(ti, frames);
                        l.fill(0.0);
                        r.fill(0.0);
                    }
                    continue;
                };
                stamps[s] = *stamp;
                let target = if bypass[s].bypassed { 0.0 } else { 1.0 };
                let events = stage.events(s);
                let capture = if telemetry.analysis.is_selected(insert) {
                    // Reading silence must not disable the graph's silent-device
                    // optimization or mark a previously silent route as active.
                    let was_dirty = mixer.dirty[ti];
                    mixer.ensure_clean(ti, frames);
                    mixer.dirty[ti] = was_dirty;
                    let (l, r) = mixer.track(ti, frames);
                    telemetry.analysis.begin(insert, l, r)
                } else {
                    None
                };
                if bypass[s].mix == 0.0 && target == 0.0 {
                    let (l, r) = mixer.track(ti, frames);
                    telemetry.analysis.finish(capture, l, r, *sample_rate);
                    continue;
                }
                if !*compensation_active && !mixer.dirty[ti] && events.is_empty() && dev.is_silent()
                {
                    bypass[s].mix = target;
                    let (l, r) = mixer.track(ti, frames);
                    telemetry.analysis.finish(capture, l, r, *sample_rate);
                    continue;
                }
                mixer.ensure_clean(ti, frames);
                // A sidechain key comes from a track earlier in plan order:
                // its signal after inserts, before its fader.
                let key = route
                    .keys
                    .iter()
                    .find(|k| k.0 == insert)
                    .map(|k| usize::from(k.1))
                    .filter(|&k| k < ti);
                if let Some(k) = key
                    && bypass[s].mix == 1.0
                    && target == 1.0
                {
                    let (kl, kr, l, r) = mixer.key_and_track(k, ti, frames);
                    if *compensation_active {
                        let key_l = &mut compensation.scratch_l[..frames];
                        let key_r = &mut compensation.scratch_r[..frames];
                        key_l.copy_from_slice(kl);
                        key_r.copy_from_slice(kr);
                        compensation.keys[ti][ii].process(key_l, key_r);
                        dev.process_keyed(&ctx, events, l, r, key_l, key_r);
                    } else {
                        dev.process_keyed(&ctx, events, l, r, kl, kr);
                    }
                    let (_, _, bad) = sanitize_peak(l, r);
                    if bad > 0 {
                        telemetry
                            .non_finite
                            .fetch_add(bad as u64, Ordering::Relaxed);
                        dev.reset();
                    }
                } else if bypass[s].mix == 1.0 && target == 1.0 {
                    let (l, r) = mixer.track(ti, frames);
                    dev.process(&ctx, events, l, r);
                    let (_, _, bad) = sanitize_peak(l, r);
                    if bad > 0 {
                        telemetry
                            .non_finite
                            .fetch_add(bad as u64, Ordering::Relaxed);
                        dev.reset();
                    }
                } else {
                    // Bypass toggled: crossfade dry and processed so the
                    // switch does not click.
                    let (l, r, dry_l, dry_r) = mixer.track_and_dry(ti, frames);
                    dry_l.copy_from_slice(l);
                    dry_r.copy_from_slice(r);
                    dev.process(&ctx, events, l, r);
                    let (_, _, bad) = sanitize_peak(l, r);
                    if bad > 0 {
                        telemetry
                            .non_finite
                            .fetch_add(bad as u64, Ordering::Relaxed);
                        dev.reset();
                    }
                    let mut m = bypass[s].mix;
                    for i in 0..frames {
                        m = slew(m, target, step);
                        l[i] = dry_l[i] + (l[i] - dry_l[i]) * m;
                        r[i] = dry_r[i] + (r[i] - dry_r[i]) * m;
                    }
                    bypass[s].mix = m;
                }
                let (l, r) = mixer.track(ti, frames);
                telemetry.analysis.finish(capture, l, r, *sample_rate);
                if let Some(db) = dev.reduction_db() {
                    telemetry.push_reduction(insert, db);
                }
            }

            if route.key_source && mixer.dirty[ti] {
                mixer.store_key(ti, frames);
            }
            let (tl, tr) = target_gains(route.gain, route.pan, route.muted);
            if !mixer.dirty[ti] {
                outputs.silence(crate::output::OutputSource::TrackMeter(route.meter), frames);
                mixer.track_ramp[ti].snap(tl, tr);
                if !*compensation_active
                    || (!compensation.outputs[ti].has_pending()
                        && !compensation.sends[ti]
                            .iter()
                            .any(|delay| delay.has_pending()))
                {
                    continue;
                }
                mixer.ensure_clean(ti, frames);
            }
            {
                let mut ramp = mixer.track_ramp[ti];
                let (l, r) = mixer.track(ti, frames);
                scale_ramped(l, r, &mut ramp, tl, tr, step);
                let (pl, pr) = (peak(l), peak(r));
                outputs.capture(crate::output::OutputSource::TrackMeter(route.meter), l, r);
                if pl > 0.0 || pr > 0.0 {
                    telemetry.push_meter(route.meter, pl, pr);
                }
                mixer.track_ramp[ti] = ramp;
            }
            if ti == master {
                continue;
            }
            // Post-fader sends, ramped across the block.
            for si in 0..plan.tracks[ti].sends.len() {
                let send = plan.tracks[ti].sends[si];
                let dest = send.dest as usize;
                if dest <= ti || dest >= n_tracks {
                    continue;
                }
                let g1 = send.gain.clamp(0.0, MAX_GAIN);
                let g0 = if send.last == crate::plan::AUTOMATION_UNSET {
                    g1
                } else {
                    send.last
                };
                plan.tracks[ti].sends[si].last = g1;
                if g0 == 0.0 && g1 == 0.0 {
                    if *compensation_active {
                        compensation.sends[ti][si].reset();
                    }
                    continue;
                }
                let was_dirty = mixer.dirty[dest];
                let (sl, sr, dl, dr) = mixer.track_pair(ti, dest, frames);
                let (sl, sr): (&[f32], &[f32]) = if *compensation_active {
                    let l = &mut compensation.scratch_l[..frames];
                    let r = &mut compensation.scratch_r[..frames];
                    l.copy_from_slice(sl);
                    r.copy_from_slice(sr);
                    compensation.sends[ti][si].process(l, r);
                    (l, r)
                } else {
                    (sl, sr)
                };
                let inc = (g1 - g0) / frames.max(1) as f32;
                let mut g = g0;
                if was_dirty {
                    for i in 0..frames {
                        g += inc;
                        dl[i] += sl[i] * g;
                        dr[i] += sr[i] * g;
                    }
                } else {
                    for i in 0..frames {
                        g += inc;
                        dl[i] = sl[i] * g;
                        dr[i] = sr[i] * g;
                    }
                    mixer.dirty[dest] = true;
                }
            }
            let route = &plan.tracks[ti];
            let Some(out) = route.output.map(usize::from) else {
                continue;
            };
            // Routing must go forward in plan order; anything else is a
            // compiler bug and is dropped rather than risk aliasing.
            if out <= ti || out >= n_tracks {
                continue;
            }
            let was_dirty = mixer.dirty[out];
            let (sl, sr, dl, dr) = mixer.track_pair(ti, out, frames);
            let (sl, sr): (&[f32], &[f32]) = if *compensation_active {
                let l = &mut compensation.scratch_l[..frames];
                let r = &mut compensation.scratch_r[..frames];
                l.copy_from_slice(sl);
                r.copy_from_slice(sr);
                compensation.outputs[ti].process(l, r);
                (l, r)
            } else {
                (sl, sr)
            };
            if was_dirty {
                for i in 0..frames {
                    dl[i] += sl[i];
                    dr[i] += sr[i];
                }
            } else {
                dl.copy_from_slice(sl);
                dr.copy_from_slice(sr);
                mixer.dirty[out] = true;
            }
        }

        // ---- master -> output ----
        if mixer.dirty[master] {
            let (l, r) = mixer.track(master, frames);
            out_l.copy_from_slice(l);
            out_r.copy_from_slice(r);
            let (pl, pr) = (peak(out_l), peak(out_r));
            telemetry.push_master(pl, pr);
        } else {
            out_l.fill(0.0);
            out_r.fill(0.0);
        }

        outputs.capture(crate::output::OutputSource::Master, out_l, out_r);

        // ---- monitoring, after the master ----
        if self.monitoring {
            if self.compensation_active {
                let l = &mut self.compensation.scratch_l[..frames];
                let r = &mut self.compensation.scratch_r[..frames];
                l.fill(0.0);
                r.fill(0.0);
                self.metronome.mix_into(l, r);
                self.compensation.click.process(l, r);
                for i in 0..frames {
                    out_l[i] += l[i];
                    out_r[i] += r[i];
                }
            } else {
                self.metronome.mix_into(out_l, out_r);
            }
            self.preview.mix_into(out_l, out_r, &mut self.retirer);
            if let Some(monitor) = self.input_monitor.as_mut() {
                monitor.mix_into(&self.input, out_l, out_r);
            }
        }
    }
}

/// Index of the first sequence event at or after engine sample `from`, given
/// the transport `origin`. Compares in the sample domain (not beats) so an
/// event that rounds to `from` is never skipped when the transport is
/// re-anchored mid-stream.
fn cursor_at(plan: &RenderPlan, origin: &Origin, from: i64, sr: f64) -> usize {
    plan.sequence
        .partition_point(|e| origin.sample_of(&plan.tempo, e.beat, sr) < from)
}

/// Stages a note-off, forgetting any note-on that had to make room for it.
fn stage_off(
    stage: &mut Staging,
    active: &mut ActiveNotes,
    telemetry: &Telemetry,
    slot: usize,
    offset: u32,
    kind: EventKind,
) {
    if let OffResult::Evicted(EventKind::NoteOn { voice, .. }) = stage.push_off(slot, offset, kind)
    {
        active.remove_runtime_voice(slot as u32, voice);
        telemetry.dropped_notes.fetch_add(1, Ordering::Relaxed);
    }
}

/// A repeated compiled note gets a fresh DSP identity on every articulation.
fn sequence_voice(next: &mut u32) -> u32 {
    let voice = *next;
    *next = if voice + 1 >= LIVE_VOICE_BASE {
        1
    } else {
        voice + 1
    };
    voice
}

/// Smallest whole beat at or after `beat`.
fn ceil_beat(beat: f64) -> i64 {
    beat.max(0.0).ceil() as i64
}

fn sane_beat(beat: f64) -> f64 {
    if beat.is_finite() { beat.max(0.0) } else { 0.0 }
}

fn clean_velocity(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// The loop range if it is usable: finite, non-empty, at least one sample.
fn valid_loop(plan: &RenderPlan, sr: f64) -> Option<(f64, f64)> {
    let (start, end) = plan.loop_range?;
    if !start.is_finite() || !end.is_finite() {
        return None;
    }
    let start = start.max(0.0);
    if end <= start {
        return None;
    }
    let len = loop_samples(&plan.tempo, start, end, sr);
    (len >= 1.0).then_some((start, end))
}

// The engine is moved into the audio callback.
const _: fn() = || {
    fn is_send<T: Send>() {}
    is_send::<Engine>();
};
