//! The control-side session.
//!
//! Owns the editable [`Document`], keeps native devices installed in engine
//! slots, compiles render plans when the document changes, and turns
//! realtime-friendly edits (gains, parameters) into direct engine commands
//! so a fader move never rebuilds the plan.

mod audio;
pub mod audition;
mod bounce;
mod compile;
mod credits;
pub mod editing;
mod export;
mod io;
mod perform;
mod plugins;
mod record;
mod samples;
pub mod template;

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use peel_document::{AudioAsset, DeviceInstance, Document, Id, Project};
use peel_dsp::SampleBuffer;
use peel_dsp::util::db_to_gain;
use peel_engine::{EngineCommand, EngineConfig, EngineHandle, RenderPlan, Slot, Telemetry};

pub use compile::{
    CompileContext, PlanIndex, PlayMode, clip_source_start_seconds, compile, same_structure,
    stretch_ratio, stretched_key,
};
pub use credits::inherit_audio_credits;
pub use export::{
    AudioExportSettings, ExportJob, ExportSettings, render as render_to_wav, render_audio,
    stem_tracks,
};
pub use io::{
    LiveTiming, MidiControlTarget, MidiEvent, MidiLearnStatus, MidiMapping, MidiPortInfo,
    MpeConfig, MpeMode, OutputChannels, OutputDeviceInfo, OutputRoute, OutputRouteStatus,
    PortSelection, RecordedMidi, RecordedPerformance, RecordedPerformanceKind, list_output_devices,
};
pub use peel_engine::{
    InputChannels, InputDeviceInfo, InputInfo, InputInjector, LatencyReport, list_input_devices,
};
pub use perform::{Slot as PerformSlot, SlotState};
pub use plugins::{
    PluginFormat, PluginHandle, PluginInfo, PluginRegistry, is_plugin_type,
    type_id_for as plugin_type_id,
};
pub use record::{MonitorStatus, RecordingStatus};
pub use samples::SampleLoader;

/// Retain each live track's meter, reclaim removed tracks and allocate only
/// free indices. Lifetime track churn must never wrap into another live bus.
fn reconcile_track_meters(project: &Project, meters: &mut HashMap<Id, u16>) {
    let active: std::collections::HashSet<_> = project.mixer.tracks.iter().map(|t| t.id).collect();
    meters.retain(|id, _| active.contains(id));
    let mut used = [false; peel_engine::telemetry::MAX_METERS];
    for track in &project.mixer.tracks {
        if let Some(&meter) = meters.get(&track.id) {
            if let Some(taken) = used.get_mut(usize::from(meter))
                && !*taken
            {
                *taken = true;
            } else {
                meters.remove(&track.id);
            }
        }
    }
    let mut free = used
        .iter()
        .enumerate()
        .filter_map(|(i, &taken)| (!taken).then_some(i as u16));
    for track in &project.mixer.tracks {
        if let std::collections::hash_map::Entry::Vacant(entry) = meters.entry(track.id)
            && let Some(meter) = free.next()
        {
            entry.insert(meter);
        }
    }
}

struct DeviceSlot {
    slot: Slot,
    type_id: String,
    params: BTreeMap<String, f32>,
    sample: Option<Id>,
    state: Option<String>,
}

/// A short user-facing notice (save result, missing device, errors).
#[derive(Clone, Debug)]
pub struct Notice {
    pub text: String,
    pub error: bool,
    pub at: Instant,
}

pub struct Session {
    pub doc: Document,
    pub path: Option<PathBuf>,
    /// The project file whose data folder holds this project's managed
    /// media while it has no file of its own (a project started from a
    /// saved template). Never written to; the first save brings the media
    /// along and clears it.
    media_base: Option<PathBuf>,
    engine: EngineHandle,
    devices: HashMap<Id, DeviceSlot>,
    free_slots: Vec<Slot>,
    next_slot: Slot,
    meters: HashMap<Id, u16>,
    analysis_device: Option<Id>,
    analysis_state: Option<(Slot, bool)>,
    plan: Option<RenderPlan>,
    index: PlanIndex,
    /// Document revision, mode and audio generation the engine was last
    /// synced to.
    synced: Option<(u64, PlayMode, u64)>,
    audio: audio::AudioState,
    mode: PlayMode,
    selected_pattern: Id,
    playing: bool,
    pattern_recording_active: bool,
    /// The engine has been seen running since the last Play.
    engine_started: bool,
    play_start: f64,
    metronome: bool,
    /// Song playback wraps at the loop range (or the arrangement end).
    looping: bool,
    samples: SampleLoader,
    plugins: PluginRegistry,
    bounce: Option<bounce::Bounce>,
    perform: Option<perform::Performance>,
    notice: Option<Notice>,
    compensation_notice: Option<String>,
    missing: Vec<String>,
    /// Audio device selection and live MIDI input (see `io.rs`).
    io: io::IoState,
    /// Audio input, armed lanes and recording (see `record.rs`).
    rec: record::RecordState,
}

impl Session {
    /// Starts the audio engine (falling back to a silent clock when no
    /// output device is available) and loads `project`.
    pub fn new(project: Project) -> Session {
        Session::new_with_audio(project, None, EngineConfig::default().buffer_frames)
    }

    /// [`Session::new`] on a chosen output device and buffer size (`None`
    /// device is the system default; see [`list_output_devices`]). A device
    /// that is missing falls back to the default with a warning.
    pub fn new_with_audio(
        project: Project,
        device: Option<String>,
        buffer_frames: Option<u32>,
    ) -> Session {
        let config = EngineConfig {
            device,
            buffer_frames,
            ..EngineConfig::default()
        };
        let engine = match EngineHandle::start(config.clone()) {
            Ok(e) => e,
            Err(e) => {
                log::error!("audio output unavailable: {e}; continuing without sound");
                EngineHandle::start_null(config.clone()).expect("null audio engine")
            }
        };
        Session::with_engine(
            project,
            engine,
            io::IoState::new(config, false),
            PluginRegistry::start_scan(),
        )
    }

    /// A session on the silent software clock (tests, headless tools). It
    /// never opens hardware MIDI ports unless asked to.
    pub fn new_silent(project: Project) -> Session {
        let config = EngineConfig::default();
        let engine = EngineHandle::start_null(config.clone()).expect("null audio engine");
        Session::with_engine(
            project,
            engine,
            io::IoState::new(config, true),
            PluginRegistry::disabled(),
        )
    }

    fn with_engine(
        project: Project,
        engine: EngineHandle,
        io: io::IoState,
        plugins: PluginRegistry,
    ) -> Session {
        log::info!(
            "audio: {} at {} Hz, block {}",
            engine.device_name(),
            engine.sample_rate(),
            engine.max_block()
        );
        let mode = PlayMode::Pattern(project.patterns.first().map_or(Id::NIL, |p| p.id));
        let mut s = Session {
            doc: Document::new(project),
            path: None,
            media_base: None,
            engine,
            devices: HashMap::new(),
            free_slots: Vec::new(),
            next_slot: 0,
            meters: HashMap::new(),
            analysis_device: None,
            analysis_state: None,
            plan: None,
            index: PlanIndex::default(),
            synced: None,
            audio: audio::AudioState::default(),
            mode,
            selected_pattern: match mode {
                PlayMode::Pattern(p) => p,
                PlayMode::Song => Id::NIL,
            },
            playing: false,
            pattern_recording_active: false,
            engine_started: false,
            play_start: 0.0,
            metronome: false,
            looping: true,
            samples: SampleLoader::new(),
            plugins,
            bounce: None,
            perform: None,
            notice: None,
            compensation_notice: None,
            missing: Vec::new(),
            io,
            rec: record::RecordState::default(),
        };
        s.init_io();
        s.sync();
        s
    }

    pub fn project(&self) -> &Project {
        self.doc.project()
    }

    // ---- per-frame upkeep ----

    /// Call once per UI frame: frees retired engine objects, installs
    /// loaded samples and syncs the engine with the document.
    pub fn tick(&mut self) {
        self.engine.collect_garbage();
        // A song without a loop stops itself at its end.
        let running = self.engine.telemetry().playing.load(Ordering::Relaxed);
        if self.playing && self.engine_started && !running {
            self.io.bridge.release_all();
            self.playing = false;
        }
        self.engine_started = self.playing && running;
        if self.plugins.poll() {
            // Plugins referenced by the project can now be created.
            self.synced = None;
        }
        for id in self.plugins.idle() {
            if let Some(slot) = self.devices.get(&id).map(|d| d.slot)
                && let Err(error) = self.engine.request_device_restart(slot)
            {
                self.notify(error, true);
            }
        }
        let error = self.engine.compensation_error().map(str::to_owned);
        if error != self.compensation_notice {
            if let Some(error) = &error {
                self.notify(error.clone(), true);
            }
            self.compensation_notice = error;
        }
        self.poll_bounce();
        self.poll_stretches();
        self.extend_performance();
        self.poll_recording();
        for (asset, result) in self.samples.poll() {
            match result {
                Ok(buf) => {
                    self.install_sample(asset, buf);
                    self.audio_loaded(asset);
                }
                Err(e) => {
                    self.audio_failed(asset);
                    self.notify(format!("Could not load sample: {e}"), true);
                }
            }
        }
        if let Some((buf, gain)) = self.samples.poll_preview() {
            let _ = self.engine.send(EngineCommand::Preview {
                sample: Some(buf),
                gain,
            });
        }
        self.sync();
        self.refresh_analysis_target();
        self.refresh_output_route_sources();
    }

    fn sync(&mut self) {
        let key = (self.doc.revision(), self.mode, self.audio.generation);
        if self.synced == Some(key) {
            return;
        }
        self.sync_devices();
        self.sync_plan();
        self.synced = Some(key);
    }

    fn all_devices(p: &Project) -> Vec<&DeviceInstance> {
        p.channels
            .iter()
            .map(|c| &c.device)
            .chain(p.mixer.tracks.iter().flat_map(|t| t.inserts.iter()))
            .collect()
    }

    fn sync_devices(&mut self) {
        let project = self.doc.project().clone();
        let wanted = Self::all_devices(&project);
        // Remove devices no longer in the document.
        let keep: std::collections::HashSet<Id> = wanted.iter().map(|d| d.id).collect();
        let stale: Vec<Id> = self
            .devices
            .keys()
            .filter(|id| !keep.contains(id))
            .copied()
            .collect();
        for id in stale {
            if let Some(d) = self.devices.remove(&id) {
                let _ = self
                    .engine
                    .send(EngineCommand::RemoveDevice { slot: d.slot });
                self.free_slots.push(d.slot);
                self.plugins.forget(id);
            }
        }
        self.missing.clear();
        for inst in wanted {
            match self.devices.get_mut(&inst.id) {
                Some(slot) if slot.type_id == inst.type_id && slot.state == inst.state => {
                    // Parameter changes.
                    let desc = peel_devices::descriptor(&inst.type_id);
                    // Removing an override (including undoing its first edit)
                    // restores the descriptor default on the live device.
                    for key in slot.params.keys().filter(|k| !inst.params.contains_key(*k)) {
                        if let Some(pd) = desc.and_then(|d| d.param_by_key(key)) {
                            let _ = self.engine.send(EngineCommand::SetParam {
                                slot: slot.slot,
                                id: pd.id,
                                value: pd.default,
                            });
                        }
                    }
                    slot.params.retain(|key, _| inst.params.contains_key(key));
                    for (k, v) in &inst.params {
                        if slot.params.get(k) != Some(v) {
                            if let Some(pd) = desc.and_then(|d| d.param_by_key(k)) {
                                let _ = self.engine.send(EngineCommand::SetParam {
                                    slot: slot.slot,
                                    id: pd.id,
                                    value: *v,
                                });
                            }
                            slot.params.insert(k.clone(), *v);
                        }
                    }
                    if slot.sample != inst.sample {
                        slot.sample = inst.sample;
                        let s = slot.slot;
                        self.request_sample(inst.sample, s);
                    }
                }
                _ => self.install_device(inst, &project),
            }
        }
        reconcile_track_meters(&project, &mut self.meters);
    }

    /// A native device, or a CLAP plugin restored from its saved state.
    pub(crate) fn create_device(
        &mut self,
        inst: &DeviceInstance,
    ) -> Option<Box<dyn peel_dsp::Device>> {
        if let Some(d) = peel_devices::create(&inst.type_id) {
            return Some(d);
        }
        if plugins::is_plugin_type(&inst.type_id) {
            let (rate, block) = (self.engine.sample_rate(), self.engine.max_block());
            match self.plugins.create(inst, rate, block) {
                Ok(d) => return Some(d),
                Err(e) => {
                    if !self.plugins.is_scanning() {
                        log::warn!("{e}");
                    }
                }
            }
        }
        None
    }

    /// Largest gain reduction (dB) a dynamics insert applied since the last
    /// call; read once per frame.
    pub fn take_reduction_db(&self, device: Id) -> f32 {
        self.devices
            .get(&device)
            .map_or(0.0, |d| self.engine.telemetry().take_reduction(d.slot))
    }

    /// Installed CLAP and VST3 plugins (empty until the background scan
    /// finishes).
    pub fn plugins(&self) -> &[PluginInfo] {
        self.plugins.plugins()
    }

    pub fn plugins_scanning(&self) -> bool {
        self.plugins.is_scanning()
    }

    /// Display text for a device parameter value: the plugin's own text for
    /// plugins, the descriptor's formatting otherwise.
    pub fn param_text(&self, device: Id, p: &peel_dsp::ParamDescriptor, value: f32) -> String {
        self.plugins
            .param_text(device, p.id, value)
            .filter(|t| !t.trim().is_empty())
            .unwrap_or_else(|| p.format(value))
    }

    /// Looks for newly installed plugins in the background.
    pub fn rescan_plugins(&mut self) {
        self.plugins.rescan();
    }

    /// Replaces the installed plugin set (tests and tools).
    pub fn set_plugins(&mut self, plugins: Vec<peel_host_clap::ClapPluginInfo>) {
        self.plugins.set(plugins);
        self.synced = None;
    }

    /// The project as it would be saved now, with every live plugin's
    /// state, leaving the document, its history and the instances alone.
    pub fn snapshot(&self) -> Project {
        let mut project = self.doc.project().clone();
        for (id, state) in self.plugins.capture_states() {
            if let Some(d) = find_device_mut(&mut project, id) {
                d.state = Some(state);
                self.plugins.sync_params(d);
            }
        }
        project
    }

    /// Stores every live plugin's state in the document (not an undo step).
    pub fn capture_plugin_states(&mut self) {
        let states = self.plugins.capture_states();
        if states.is_empty() {
            return;
        }
        // This state already belongs to the live instance. Updating the
        // mirror prevents a later document edit from reloading that instance.
        for (id, state) in &states {
            if let Some(slot) = self.devices.get_mut(id) {
                slot.state = Some(state.clone());
            }
        }
        let plugins = &self.plugins;
        self.doc.patch(|p| {
            for (id, state) in states {
                if let Some(d) = find_device_mut(p, id) {
                    d.state = Some(state);
                    plugins.sync_params(d);
                }
            }
        });
    }

    /// Whether the plugin behind `device` can open its own window.
    pub fn plugin_has_editor(&self, device: Id) -> bool {
        self.plugins
            .handle(device)
            .is_some_and(PluginHandle::has_editor)
    }

    /// Whether the plugin window for `device` is showing.
    pub fn plugin_editor_open(&self, device: Id) -> bool {
        self.plugins
            .handle(device)
            .is_some_and(PluginHandle::editor_open)
    }

    /// Opens (or brings forward) the plugin's own window.
    pub fn open_plugin_editor(&mut self, device: Id) -> Result<(), String> {
        self.plugins
            .handle(device)
            .ok_or_else(|| "This plugin is not loaded".to_string())?
            .open_editor()
    }

    pub fn close_plugin_editor(&self, device: Id) {
        if let Some(h) = self.plugins.handle(device) {
            h.close_editor();
        }
    }

    fn install_device(&mut self, inst: &DeviceInstance, project: &Project) {
        if self.analysis_device == Some(inst.id) {
            self.engine.telemetry().analysis.invalidate();
        }
        // Prepare and validate the replacement before retiring a working slot.
        let previous_slot = self.devices.get(&inst.id).map(|old| old.slot);
        let Some(mut device) = self.create_device(inst) else {
            self.missing.push(inst.type_id.clone());
            return;
        };
        let desc = device.descriptor();
        for (k, v) in &inst.params {
            if let Some(p) = desc.param_by_key(k) {
                device.set_param(p.id, *v);
            }
        }
        device.prepare(self.engine.sample_rate(), self.engine.max_block());
        let slot = previous_slot.unwrap_or_else(|| {
            self.free_slots.pop().unwrap_or_else(|| {
                let s = self.next_slot;
                self.next_slot += 1;
                s
            })
        });
        if self
            .engine
            .send(EngineCommand::InstallDevice { slot, device })
            .is_err()
        {
            if previous_slot.is_none() {
                self.free_slots.push(slot);
            }
            let error = self
                .engine
                .compensation_error()
                .unwrap_or("The audio command queue is full")
                .to_string();
            self.notify(error, true);
            return;
        }
        self.devices.insert(
            inst.id,
            DeviceSlot {
                slot,
                type_id: inst.type_id.clone(),
                params: inst.params.clone(),
                sample: inst.sample,
                state: inst.state.clone(),
            },
        );
        if inst.sample.is_some() {
            let _ = project;
            self.request_sample(inst.sample, slot);
        }
    }

    fn request_sample(&mut self, asset: Option<Id>, slot: Slot) {
        match asset {
            None => {
                let _ = self
                    .engine
                    .send(EngineCommand::SetSample { slot, sample: None });
            }
            Some(id) => {
                if let Some(buf) = self.samples.cached(id) {
                    let _ = self.engine.send(EngineCommand::SetSample {
                        slot,
                        sample: Some(buf),
                    });
                } else if let Some(asset) = self.doc.project().asset(id).cloned() {
                    let path = self.asset_path(&asset);
                    self.samples.request(id, path, self.engine.sample_rate());
                }
            }
        }
    }

    fn install_sample(&mut self, asset: Id, buf: Arc<SampleBuffer>) {
        let p = self.doc.project();
        let users: Vec<Slot> = Self::all_devices(p)
            .into_iter()
            .filter(|d| d.sample == Some(asset))
            .filter_map(|d| self.devices.get(&d.id).map(|s| s.slot))
            .collect();
        for slot in users {
            let _ = self.engine.send(EngineCommand::SetSample {
                slot,
                sample: Some(buf.clone()),
            });
        }
    }

    /// Filesystem location of an asset.
    pub fn asset_path(&self, asset: &AudioAsset) -> PathBuf {
        if asset.managed
            && let Some(p) = self.media_source()
        {
            return peel_project::resolve_asset_path(p, asset);
        }
        PathBuf::from(&asset.path)
    }

    /// The project file whose data folder holds the managed media: the
    /// project's own file, or the template it started from.
    pub fn media_source(&self) -> Option<&Path> {
        self.path.as_deref().or(self.media_base.as_deref())
    }

    /// Cached decoded sample for an asset (for waveform drawing).
    pub fn sample(&self, asset: Id) -> Option<Arc<SampleBuffer>> {
        self.samples.cached(asset)
    }

    fn sync_plan(&mut self) {
        let slots: HashMap<Id, Slot> = self.devices.iter().map(|(id, d)| (*id, d.slot)).collect();
        // A pattern-mode selection that no longer exists falls back.
        if let PlayMode::Pattern(pid) = self.mode
            && self.doc.project().pattern(pid).is_none()
        {
            self.mode = PlayMode::Pattern(
                self.doc
                    .project()
                    .patterns
                    .first()
                    .map_or(Id::NIL, |p| p.id),
            );
        }
        let samples = self.clip_samples();
        let performance = self.performance_project();
        let (mut plan, index) = compile(
            performance.as_ref().unwrap_or(self.doc.project()),
            &CompileContext {
                mode: self.mode,
                slots: &slots,
                meters: &self.meters,
                samples: &samples,
            },
        );
        if self.mode == PlayMode::Song && !self.looping {
            // Without looping the song plays to its end and stops.
            plan.loop_range = None;
        }
        if performance.is_some() {
            // Launches play on and on; the plan is extended as it goes.
            plan.loop_range = None;
            plan.length_beats = f64::INFINITY;
        }
        if self.recording_free_runs() {
            // A take is one straight recording: the transport must not wrap
            // at the loop or stop at the end of the arrangement under it.
            plan.loop_range = None;
            plan.length_beats = f64::INFINITY;
        }
        match &self.plan {
            Some(old) if same_structure(old, &plan) => {
                for (i, (a, b)) in old.channels.iter().zip(&plan.channels).enumerate() {
                    let channel = i as u16;
                    if a.gain != b.gain {
                        let _ = self.engine.send(EngineCommand::SetChannelGain {
                            channel,
                            gain: b.gain,
                        });
                    }
                    if a.pan != b.pan {
                        let _ = self.engine.send(EngineCommand::SetChannelPan {
                            channel,
                            pan: b.pan,
                        });
                    }
                    if a.muted != b.muted {
                        let _ = self.engine.send(EngineCommand::SetChannelMute {
                            channel,
                            muted: b.muted,
                        });
                    }
                }
                for (i, (a, b)) in old.tracks.iter().zip(&plan.tracks).enumerate() {
                    let track = i as u16;
                    if a.gain != b.gain {
                        let _ = self.engine.send(EngineCommand::SetTrackGain {
                            track,
                            gain: b.gain,
                        });
                    }
                    if a.pan != b.pan {
                        let _ = self
                            .engine
                            .send(EngineCommand::SetTrackPan { track, pan: b.pan });
                    }
                    if a.muted != b.muted {
                        let _ = self.engine.send(EngineCommand::SetTrackMute {
                            track,
                            muted: b.muted,
                        });
                    }
                    for (si, (x, y)) in a.sends.iter().zip(&b.sends).enumerate() {
                        if x.gain != y.gain {
                            let _ = self.engine.send(EngineCommand::SetSendGain {
                                track,
                                send: si as u16,
                                gain: y.gain,
                            });
                        }
                    }
                }
                self.plan = Some(plan);
            }
            _ => {
                if self
                    .engine
                    .send(EngineCommand::SetPlan(Box::new(plan.clone())))
                    .is_err()
                {
                    let error = self
                        .engine
                        .compensation_error()
                        .unwrap_or("The audio command queue is full")
                        .to_string();
                    self.notify(error, true);
                    return;
                }
                self.plan = Some(plan);
            }
        }
        self.index = index;
    }

    // ---- notices ----

    pub fn notify(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error {
            log::warn!("{text}");
        }
        self.notice = Some(Notice {
            text,
            error,
            at: Instant::now(),
        });
    }

    pub fn notice(&self) -> Option<&Notice> {
        self.notice
            .as_ref()
            .filter(|n| n.at.elapsed().as_secs_f32() < 4.0)
    }

    /// Device types referenced by the project that are not installed.
    pub fn missing_devices(&self) -> &[String] {
        &self.missing
    }

    // ---- transport ----

    pub fn mode(&self) -> PlayMode {
        self.mode
    }

    /// The pattern being edited (and played in pattern mode).
    pub fn selected_pattern(&self) -> Id {
        if self.doc.project().pattern(self.selected_pattern).is_some() {
            self.selected_pattern
        } else {
            self.doc
                .project()
                .patterns
                .first()
                .map_or(Id::NIL, |p| p.id)
        }
    }

    pub fn select_pattern(&mut self, id: Id) {
        self.selected_pattern = id;
        if let PlayMode::Pattern(_) = self.mode {
            self.mode = PlayMode::Pattern(id);
        }
    }

    pub fn set_mode(&mut self, mode: PlayMode) {
        if let PlayMode::Pattern(p) = mode {
            self.selected_pattern = p;
        }
        if self.mode != mode {
            self.io.bridge.release_all();
            let was_song = self.mode == PlayMode::Song;
            self.mode = mode;
            if was_song != (mode == PlayMode::Song) {
                // A song position means nothing inside a pattern, and the
                // other way round: both start from the top.
                self.sync();
                self.seek(0.0);
            }
        }
    }

    /// A desktop pattern take is buffered until its final undoable edit.
    pub fn pattern_recording_active(&self) -> bool {
        self.pattern_recording_active
    }

    /// Updated by the pattern recorder, independently of audio input recording.
    pub fn set_pattern_recording_active(&mut self, active: bool) {
        self.pattern_recording_active = active;
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    pub fn play(&mut self) {
        self.sync();
        let at = self.position();
        // A playhead left at or past the end (a finished song, or a pattern
        // shorter than the last one) plays from the top.
        let ended = self
            .plan
            .as_ref()
            .is_some_and(|p| p.length_beats.is_finite() && at >= p.length_beats);
        if ended {
            self.seek(0.0);
        } else {
            self.play_start = at;
        }
        let _ = self.engine.send(EngineCommand::Play);
        self.playing = true;
    }

    /// Stops and returns to where playback started.
    pub fn stop(&mut self) {
        self.io.bridge.release_all();
        // Stopping the transport ends a recording: its clips are claimed with
        // `stop_recording`.
        self.finish_recording(true);
        let back = if self.playing { self.play_start } else { 0.0 };
        let _ = self.engine.send(EngineCommand::Stop { return_to: back });
        self.playing = false;
    }

    pub fn toggle_play(&mut self) {
        if self.perform.is_some() {
            // Stopping a performance ends it (a capture is kept).
            self.stop_performance();
        } else if self.playing {
            self.stop()
        } else {
            self.play()
        }
    }

    pub fn seek(&mut self, beat: f64) {
        self.io.bridge.release_all();
        let _ = self.engine.send(EngineCommand::Seek {
            beat: beat.max(0.0),
        });
        self.play_start = beat.max(0.0);
    }

    pub fn panic(&mut self) {
        self.io.bridge.release_all();
        let _ = self.engine.send(EngineCommand::Panic);
    }

    /// Current mixer/plugin latency, separate from hardware buffer latency.
    pub fn compensation_latency_samples(&self) -> u32 {
        self.engine.compensation_latency_samples()
    }

    pub fn compensation_error(&self) -> Option<&str> {
        self.engine.compensation_error()
    }

    /// Playhead in quarter notes.
    pub fn position(&self) -> f64 {
        self.engine.telemetry().beat()
    }

    /// Where the last seek or play started (Stop returns here).
    pub fn start_position(&self) -> f64 {
        self.play_start
    }

    /// Song playback loops (pattern playback always does).
    pub fn looping(&self) -> bool {
        self.looping
    }

    pub fn set_looping(&mut self, on: bool) {
        if self.looping != on {
            self.looping = on;
            self.synced = None;
            self.sync();
        }
    }

    /// End of what plays: the pattern length, or the arrangement rounded up
    /// to whole bars.
    pub fn play_end(&self) -> f64 {
        let p = self.doc.project();
        match self.mode {
            PlayMode::Pattern(pid) => p.pattern(pid).map_or(4.0, |pt| pt.length.as_f64()),
            PlayMode::Song => {
                let bar = p.time_signature.bar_length();
                p.arrangement().end().ceil_to(bar).max(bar).as_f64()
            }
        }
    }

    /// Moves the playhead by `beats` (negative rewinds), kept inside what
    /// plays. While playing, Stop still returns to where play began.
    pub fn nudge(&mut self, beats: f64) {
        let to = (self.position() + beats).clamp(0.0, self.play_end());
        let start = self.play_start;
        self.seek(to);
        if self.playing {
            self.play_start = start;
        }
    }

    pub fn metronome(&self) -> bool {
        self.metronome
    }

    pub fn set_metronome(&mut self, on: bool) {
        self.metronome = on;
        let _ = self.engine.send(EngineCommand::Metronome { enabled: on });
    }

    // ---- live input and audition ----

    /// Plays a note on a channel's instrument immediately.
    pub fn live_note(&mut self, channel: Id, key: u8, velocity: f32, on: bool) {
        let Some(ch) = self.doc.project().channel(channel) else {
            return;
        };
        if let Some(d) = self.devices.get(&ch.device.id) {
            let accepted = self.io.bridge.note(d.slot, key, velocity, on);
            if accepted && on && self.live_target() == Some(channel) {
                self.io.bridge.clear_key_pressure(key);
            }
        }
    }

    /// Auditions an audio file on the preview route.
    pub fn preview_file(&mut self, path: &Path) {
        self.samples
            .request_preview(path.to_path_buf(), self.engine.sample_rate(), 0.7);
    }

    /// Plays one note of a native instrument on the preview route, without
    /// adding it to the project. Like a file preview it is left out of
    /// exports and replaces whatever was previewing.
    pub fn preview_device(&mut self, type_id: &str, preset: Option<&str>) {
        self.samples
            .request_audition(type_id, preset, self.engine.sample_rate(), 0.8);
    }

    pub fn stop_preview(&mut self) {
        self.samples.cancel_preview();
        let _ = self.engine.send(EngineCommand::Preview {
            sample: None,
            gain: 0.0,
        });
    }

    // ---- parameters ----

    /// Sets a device parameter as part of a gesture: the engine hears it
    /// immediately and the document records one undo step per gesture.
    pub fn set_param(&mut self, device: Id, key: &str, value: f32, gesture: u64) {
        let label = peel_devices::descriptor(self.device_type(device).unwrap_or_default().as_str())
            .and_then(|d| d.param_by_key(key))
            .map_or_else(
                || "Change parameter".to_string(),
                |p| format!("Change {}", p.name),
            );
        self.doc.edit_gesture(gesture, label, |p| {
            let old_piece = find_device(p, device)
                .and_then(|d| d.params.get("kit.piece"))
                .copied()
                .unwrap_or(0.0);
            if let Some(d) = find_device_mut(p, device) {
                d.params.insert(key.to_string(), value);
            }
            if key == "kit.piece" {
                update_recorded_kit_mapping(p, device, old_piece);
            }
        });
        if let Some(slot) = self.devices.get_mut(&device) {
            if let Some(pd) =
                peel_devices::descriptor(&slot.type_id).and_then(|d| d.param_by_key(key))
            {
                let _ = self.engine.send(EngineCommand::SetParam {
                    slot: slot.slot,
                    id: pd.id,
                    value,
                });
            }
            slot.params.insert(key.to_string(), value);
        }
    }

    fn device_type(&self, device: Id) -> Option<String> {
        find_device(self.doc.project(), device).map(|d| d.type_id.clone())
    }

    // ---- selected-insert analysis ----

    /// Selects one mixer insert for pre/post waveform and spectrum capture.
    /// Re-selecting the same insert is cheap and preserves its capture window.
    /// Instruments and unknown IDs are rejected. `None` disables the tap.
    pub fn set_analysis_device(&mut self, device: Option<Id>) -> bool {
        let valid = device.is_none_or(|id| {
            self.project()
                .mixer
                .tracks
                .iter()
                .any(|t| t.inserts.iter().any(|d| d.id == id))
        });
        if !valid {
            return false;
        }
        if self.analysis_device != device {
            self.analysis_device = device;
            self.analysis_state = None;
            self.engine.telemetry().analysis.invalidate();
        }
        self.refresh_analysis_target();
        self.refresh_output_route_sources();
        true
    }

    fn refresh_analysis_target(&mut self) {
        let state = self.analysis_device.and_then(|id| {
            let inst = self
                .project()
                .mixer
                .tracks
                .iter()
                .flat_map(|t| &t.inserts)
                .find(|d| d.id == id)?;
            self.devices.get(&id).map(|d| (d.slot, inst.bypass))
        });
        if self.analysis_state != state {
            self.engine.telemetry().analysis.invalidate();
            self.analysis_state = state;
        }
        self.engine
            .telemetry()
            .analysis
            .set_slot(state.map(|s| s.0));
    }

    /// One bounded snapshot attempt; `None` means no selected insert audio or
    /// a concurrent callback write. Analyze this frame on the control thread.
    pub fn analysis_snapshot(&self) -> Option<peel_engine::analysis::AnalysisFrame> {
        self.engine.telemetry().analysis.snapshot()
    }

    // ---- telemetry ----

    pub fn telemetry(&self) -> &Arc<Telemetry> {
        self.engine.telemetry()
    }

    /// Peak of a mixer track since the last call.
    pub fn take_track_meter(&self, track: Id) -> (f32, f32) {
        self.meters
            .get(&track)
            .map_or((0.0, 0.0), |&m| self.engine.telemetry().take_meter(m))
    }

    /// Output peak of a channel's instrument since the last call.
    pub fn take_channel_activity(&self, channel: Id) -> f32 {
        let Some(c) = self.doc.project().channel(channel) else {
            return 0.0;
        };
        self.devices
            .get(&c.device.id)
            .map_or(0.0, |d| self.engine.telemetry().take_slot(d.slot))
    }

    pub fn cpu_load(&self) -> f32 {
        self.engine.telemetry().cpu_load()
    }

    pub fn device_name(&self) -> &str {
        self.engine.device_name()
    }

    pub fn sample_rate(&self) -> f32 {
        self.engine.sample_rate()
    }

    // ---- files ----

    /// Saves to `path` (or the current path).
    pub fn save(&mut self, path: Option<PathBuf>) -> Result<PathBuf, String> {
        let path = path.or_else(|| self.path.clone()).ok_or("no file chosen")?;
        let path = if path.extension().is_some_and(|e| e == "peelproj") {
            path
        } else {
            path.with_extension("peelproj")
        };
        let mut project = self.doc.project().clone();
        if project.meta.title == "Untitled" || project.meta.title.is_empty() {
            project.meta.title = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            let title = project.meta.title.clone();
            self.doc.edit("Rename project", |p| p.meta.title = title);
        }
        self.capture_plugin_states();
        // A new file brings the managed media along from where it is now.
        let from = self.media_source().map(Path::to_path_buf);
        peel_project::save_as(self.doc.project(), from.as_deref(), &path)
            .map_err(|e| e.to_string())?;
        self.doc.mark_saved();
        self.path = Some(path.clone());
        self.media_base = None;
        self.notify(
            format!(
                "Saved {}",
                path.file_name()
                    .map(|n| n.to_string_lossy())
                    .unwrap_or_default()
            ),
            false,
        );
        Ok(path)
    }

    /// Opens a project, replacing the current one.
    pub fn open(&mut self, path: &Path) -> Result<(), String> {
        let loaded = peel_project::load(path).map_err(|e| e.to_string())?;
        for w in &loaded.warnings {
            log::warn!("{w}");
        }
        self.replace_project(loaded.project);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    /// Saves a copy of the project, with live plugin state and all of its
    /// media, as the template `name` in the library at `dir`. The project,
    /// its file, title, history and saved state are not touched.
    pub fn save_template(
        &self,
        dir: &Path,
        name: &str,
    ) -> Result<PathBuf, peel_project::TemplateError> {
        peel_project::save_template(&self.snapshot(), self.media_source(), dir, name)
    }

    /// Replaces the document (new project, template, open).
    pub fn replace_project(&mut self, project: Project) {
        self.replace_project_with_media(project, None);
    }

    /// Replaces the document with a project that has no file yet but whose
    /// managed media lives with `media_base` (a saved template's root).
    pub fn replace_project_with_media(&mut self, project: Project, media_base: Option<PathBuf>) {
        self.pattern_recording_active = false;
        self.stop();
        self.forget_lanes();
        self.doc.replace(project);
        self.path = None;
        self.media_base = media_base;
        self.mode = PlayMode::Pattern(
            self.doc
                .project()
                .patterns
                .first()
                .map_or(Id::NIL, |p| p.id),
        );
        if let PlayMode::Pattern(p) = self.mode {
            self.selected_pattern = p;
        }
        self.synced = None;
        self.plan = None;
        self.audio.failed.clear();
        let keep: std::collections::HashSet<Id> =
            self.doc.project().assets.iter().map(|a| a.id).collect();
        self.samples.retain_assets(&keep);
        self.sync();
    }

    /// Renders one audio file per mixer track that carries sound (each track
    /// soloed, so it keeps its buses and returns) on a background thread.
    /// Files go next to `settings.render.path` as `<name> - <track>.<format>`.
    pub fn export_stems(&mut self, settings: impl Into<AudioExportSettings>) -> ExportJob {
        let settings = settings.into();
        if let Err(error) = settings.validate() {
            return ExportJob::failed(error);
        }
        self.capture_plugin_states();
        let project = self.doc.project().clone();
        let base = settings.render.path.with_extension("");
        let stem = base
            .file_name()
            .map_or("Export".to_string(), |s| s.to_string_lossy().into_owned());
        let dir = base.parent().map(Path::to_path_buf).unwrap_or_default();
        let mut renders = Vec::new();
        let mut handles = Vec::new();
        let mut used_names = std::collections::HashSet::new();
        for track in export::stem_tracks(&project) {
            let mut p = project.clone();
            for t in &mut p.mixer.tracks {
                t.solo = t.id == track;
            }
            let name = p
                .mixer
                .track(track)
                .map_or("Track".to_string(), |t| t.name.clone());
            let name = export::file_safe(&name);
            let mut unique = name.clone();
            let mut suffix = 2;
            while !used_names.insert(unique.to_lowercase()) {
                unique = format!("{name} ({suffix})");
                suffix += 1;
            }
            let path = dir.join(format!("{stem} - {unique}.{}", settings.format.extension()));
            let (prebuilt, mut h) = self.offline_plugins(&p, settings.render.sample_rate);
            handles.append(&mut h);
            renders.push(export::Render {
                project: p,
                prebuilt,
                settings: AudioExportSettings {
                    render: ExportSettings {
                        path,
                        ..settings.render.clone()
                    },
                    ..settings.clone()
                },
            });
        }
        let samples = self.cached_project_samples();
        ExportJob::start_many(
            renders,
            samples,
            self.export_loads(),
            handles,
            settings.render.sample_rate,
            Some(dir),
        )
    }

    /// Plugin instances for an offline render of `p`, made here on the main
    /// thread from their current state.
    fn offline_plugins(&mut self, p: &Project, rate: u32) -> (export::Prebuilt, Vec<PluginHandle>) {
        let mut prebuilt = export::Prebuilt::new();
        let mut handles = Vec::new();
        let instances = p
            .channels
            .iter()
            .map(|c| &c.device)
            .chain(p.mixer.tracks.iter().flat_map(|t| t.inserts.iter()))
            .filter(|d| plugins::is_plugin_type(&d.type_id));
        for inst in instances {
            match self.plugins.create_offline(inst, rate as f32, 512) {
                Ok((dev, handle)) => {
                    prebuilt.insert(inst.id, dev);
                    handles.push(handle);
                }
                Err(e) => log::warn!("export: {e}"),
            }
        }
        (prebuilt, handles)
    }

    fn cached_project_samples(&self) -> HashMap<Id, Arc<SampleBuffer>> {
        self.doc
            .project()
            .assets
            .iter()
            .filter_map(|a| self.samples.cached(a.id).map(|b| (a.id, b)))
            .collect()
    }

    /// Renders the arrangement to an audio file on a background thread.
    pub fn export(&mut self, settings: impl Into<AudioExportSettings>) -> ExportJob {
        let settings = settings.into();
        if let Err(error) = settings.validate() {
            return ExportJob::failed(error);
        }
        // Plugins get their own instances, built here on the main thread
        // from their current state.
        self.capture_plugin_states();
        let project = self.doc.project().clone();
        let (prebuilt, handles) = self.offline_plugins(&project, settings.render.sample_rate);
        let samples = self.cached_project_samples();
        ExportJob::start(
            project,
            samples,
            self.export_loads(),
            prebuilt,
            handles,
            settings,
        )
    }

    /// Imports an audio file as an asset referenced in place (not copied).
    pub fn reference_audio_file(&mut self, path: &Path) -> Result<AudioAsset, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        let hash = blake3::hash(&bytes).to_hex().to_string();
        if let Some(existing) = self.doc.project().assets.iter().find(|a| a.hash == hash) {
            let mut asset = existing.clone();
            // Keep the incoming path available for sidecar attribution even
            // when the bytes are already registered under another path.
            asset.path = path.to_string_lossy().into_owned();
            return Ok(asset);
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(AudioAsset {
            id: Id::new(),
            hash,
            name,
            path: path.to_string_lossy().into_owned(),
            managed: false,
            sample_rate: 0,
            channels: 0,
            frames: 0,
        })
    }

    /// Gain to apply for a dB fader value (helper for UIs).
    pub fn db_to_gain(db: f32) -> f32 {
        db_to_gain(db)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.engine.send(EngineCommand::Panic);
    }
}

/// Keeps step notes at the recorded pitch when switching a channel between
/// the GM kit map and one selected piece. Only notes on the channel's old
/// root move; other piano-roll notes and their expression stay untouched.
/// Call inside the same document edit that changes `kit.piece`.
pub fn update_recorded_kit_mapping(p: &mut Project, device: Id, old_piece: f32) {
    let Some(ch) = p
        .channels
        .iter_mut()
        .find(|ch| ch.device.id == device && ch.device.type_id == "peel.drum.acoustic")
    else {
        return;
    };
    let piece = ch.device.params.get("kit.piece").copied().unwrap_or(0.0);
    if (old_piece >= 0.5) == (piece >= 0.5) {
        return;
    }
    let old_root = ch.root_key;
    let root = if piece >= 0.5 { 60 } else { 36 };
    let channel = ch.id;
    ch.root_key = root;
    for pat in &mut p.patterns {
        if !pat
            .notes
            .iter()
            .any(|n| n.channel == channel && n.key == old_root)
        {
            continue;
        }
        let pat = Arc::make_mut(pat);
        for note in &mut pat.notes {
            if note.channel == channel && note.key == old_root {
                note.key = root;
            }
        }
        pat.sort_notes();
    }
}

/// Finds a device instance anywhere in the project.
pub fn find_device(p: &Project, id: Id) -> Option<&DeviceInstance> {
    p.channels
        .iter()
        .map(|c| &c.device)
        .chain(p.mixer.tracks.iter().flat_map(|t| t.inserts.iter()))
        .find(|d| d.id == id)
}

pub fn find_device_mut(p: &mut Project, id: Id) -> Option<&mut DeviceInstance> {
    if let Some(c) = p.channels.iter_mut().find(|c| c.device.id == id) {
        return Some(&mut c.device);
    }
    p.mixer
        .tracks
        .iter_mut()
        .flat_map(|t| t.inserts.iter_mut())
        .find(|d| d.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_looping_can_be_turned_off() {
        let mut s = Session::new_silent(template::beat_and_melody());
        s.set_mode(PlayMode::Song);
        s.sync();
        let end = s.play_end();
        assert!(end > 0.0);
        assert!(s.plan.as_ref().unwrap().loop_range.is_some());
        s.set_looping(false);
        assert!(s.plan.as_ref().unwrap().loop_range.is_none());
        assert_eq!(s.plan.as_ref().unwrap().length_beats, end);
        s.set_looping(true);
        assert!(s.plan.as_ref().unwrap().loop_range.is_some());
    }

    #[test]
    fn save_and_reopen_round_trip() {
        let dir = std::env::temp_dir().join(format!("peel-session-test-{}", Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("Groove.peelproj");
        let mut a = Session::new_silent(template::beat_and_melody());
        let saved = a.save(Some(path.clone())).expect("save");
        assert_eq!(saved, path);
        assert!(!a.doc.is_dirty());
        assert_eq!(a.project().meta.title, "Groove");

        let mut b = Session::new_silent(template::empty());
        b.open(&path).expect("open");
        let (pa, pb) = (a.project(), b.project());
        assert_eq!(pa.channels, pb.channels);
        assert_eq!(pa.patterns, pb.patterns);
        assert_eq!(pa.arrangements, pb.arrangements);
        assert_eq!(pa.mixer, pb.mixer);
        assert_eq!(pa.bpm(), pb.bpm());
        assert!(b.missing_devices().is_empty(), "{:?}", b.missing_devices());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_sidechained_compressor_hears_its_key_track_first() {
        let mut project = template::beat_and_melody();
        let kick_track = project
            .channels
            .iter()
            .find(|c| c.name == "Kick")
            .unwrap()
            .track;
        let synth_track = project
            .channels
            .iter()
            .find(|c| c.name == "Synth")
            .unwrap()
            .track;
        let mut comp = template::device("peel.fx.compressor", None);
        comp.sidechain = Some(kick_track);
        let comp_id = comp.id;
        project
            .mixer
            .track_mut(synth_track)
            .unwrap()
            .inserts
            .insert(0, comp);
        assert!(
            peel_document::validate(&project)
                .iter()
                .all(|i| !i.is_error())
        );
        let s = Session::new_silent(project);
        let plan = s.plan.as_ref().expect("plan").clone();
        let slot = s.devices[&comp_id].slot;
        let (ti, route) = plan
            .tracks
            .iter()
            .enumerate()
            .find(|(_, r)| r.inserts.contains(&slot))
            .expect("the synth track");
        let (_, key) = route
            .keys
            .iter()
            .copied()
            .find(|k| k.0 == slot)
            .expect("keyed");
        assert!(usize::from(key) < ti, "the kick track is processed first");
    }

    #[test]
    fn a_soloed_track_still_pumps_with_a_muted_kick() {
        let dir = std::env::temp_dir().join(format!("peel-sc-test-{}", Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut project = template::beat_and_melody();
        let kick_track = project
            .channels
            .iter()
            .find(|c| c.name == "Kick")
            .unwrap()
            .track;
        let synth_track = project
            .channels
            .iter()
            .find(|c| c.name == "Synth")
            .unwrap()
            .track;
        let render = |project: &Project, name: &str| {
            let out = dir.join(name);
            let settings = ExportSettings {
                path: out.clone(),
                sample_rate: 48_000,
                bits: 32,
                tail_seconds: 0.0,
            };
            let progress = std::sync::atomic::AtomicU32::new(0);
            let cancel = std::sync::atomic::AtomicBool::new(false);
            render_to_wav(project, &HashMap::new(), &settings, &progress, &cancel).expect("render");
            peel_media::decode_file(&out)
                .expect("decode")
                .left()
                .to_vec()
        };
        project.mixer.track_mut(synth_track).unwrap().solo = true;
        let plain = render(&project, "plain.wav");
        let mut comp = template::device("peel.fx.compressor", Some("Sidechain Duck"));
        comp.sidechain = Some(kick_track);
        project
            .mixer
            .track_mut(synth_track)
            .unwrap()
            .inserts
            .insert(0, comp);
        let ducked = render(&project, "ducked.wav");
        // Level just after each kick against the same moment without the
        // compressor, over the first eight bars.
        let beat = 48_000.0 * 60.0 / 118.0;
        let rms = |x: &[f32], b: usize| {
            let a = (b as f64 * beat) as usize + 1_000;
            let s = &x[a..a + 2_400];
            (s.iter().map(|v| v * v).sum::<f32>() / s.len() as f32).sqrt()
        };
        let (mut on, mut off) = (0.0, 0.0);
        for b in 0..32 {
            on += rms(&ducked, b);
            off += rms(&plain, b);
        }
        assert!(off > 0.01, "the synth is playing: {off}");
        assert!(on < off * 0.5, "ducked after each kick: {on} vs {off}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn stems_are_one_file_per_sounding_track_and_sum_to_the_mix() {
        let dir = std::env::temp_dir().join(format!("peel-stems-test-{}", Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut project = template::beat_and_melody();
        // No master processing, so the stems add up to the mix exactly.
        project.mixer.tracks[0].inserts.clear();
        for t in project.mixer.tracks.iter_mut() {
            t.inserts
                .retain(|d| !d.type_id.contains("reverb") && !d.type_id.contains("delay"));
        }
        let mut s = Session::new_silent(project);
        let settings = |name: &str| ExportSettings {
            path: dir.join(name),
            sample_rate: 48_000,
            bits: 32,
            tail_seconds: 0.0,
        };
        let wait = |job: ExportJob| loop {
            if let Some(r) = job.take_result() {
                return r;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        let mix = wait(s.export(settings("Song.wav"))).expect("mix");
        let folder = wait(s.export_stems(settings("Song.wav"))).expect("stems");
        assert_eq!(folder, dir);
        let tracks = stem_tracks(s.project());
        let mut files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().into_owned()))
            .filter(|n| n.starts_with("Song - "))
            .collect();
        files.sort();
        assert_eq!(files.len(), tracks.len(), "{files:?}");
        assert!(files.iter().any(|f| f == "Song - Kick.wav"), "{files:?}");
        let full = peel_media::decode_file(&mix).unwrap();
        let mut sum = vec![0.0f32; full.left().len()];
        for f in &files {
            let b = peel_media::decode_file(dir.join(f)).unwrap();
            for (acc, x) in sum.iter_mut().zip(b.left()) {
                *acc += x;
            }
        }
        let err = sum
            .iter()
            .zip(full.left())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(err < 1e-3, "stems sum to the mix (max error {err})");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_ducking_compressor_reports_gain_reduction() {
        let mut project = template::beat_and_melody();
        let kick = project
            .channels
            .iter()
            .find(|c| c.name == "Kick")
            .unwrap()
            .track;
        let synth = project
            .channels
            .iter()
            .find(|c| c.name == "Synth")
            .unwrap()
            .track;
        let mut comp = template::device("peel.fx.compressor", Some("Sidechain Duck"));
        comp.sidechain = Some(kick);
        let id = comp.id;
        project
            .mixer
            .track_mut(synth)
            .unwrap()
            .inserts
            .insert(0, comp);
        let mut s = Session::new_silent(project);
        s.set_mode(PlayMode::Song);
        s.play();
        let start = Instant::now();
        let mut most = 0.0f32;
        while start.elapsed().as_secs_f32() < 5.0 && most < 6.0 {
            s.tick();
            most = most.max(s.take_reduction_db(id));
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(most >= 6.0, "the kick pulls the synth down: {most} dB");
    }

    #[test]
    fn every_template_renders_at_a_sensible_level() {
        let dir = std::env::temp_dir().join(format!("peel-tpl-test-{}", Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, make) in template::templates() {
            let out = dir.join("t.wav");
            let settings = ExportSettings {
                path: out.clone(),
                sample_rate: 48_000,
                bits: 32,
                tail_seconds: 0.0,
            };
            let progress = std::sync::atomic::AtomicU32::new(0);
            let cancel = std::sync::atomic::AtomicBool::new(false);
            render_to_wav(&make(), &HashMap::new(), &settings, &progress, &cancel).expect("render");
            let buf = peel_media::decode_file(&out).expect("decode");
            let x = buf.left();
            let peak = x.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let rms = (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
            eprintln!(
                "{name}: peak {peak:.3} rms {rms:.3} ({:.1} s)",
                x.len() as f32 / 48_000.0
            );
            assert!(x.iter().all(|v| v.is_finite()), "{name}");
            assert!(peak > 0.3 && peak <= 1.0, "{name}: peak {peak}");
            assert!(rms > 0.04, "{name}: rms {rms}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rendering_a_clip_to_audio_sounds_the_same() {
        use peel_document::{ClipContent, Time};
        let dir = std::env::temp_dir().join(format!("peel-bounce-test-{}", Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut s = Session::new_silent(template::beat_and_melody());
        s.set_recordings_dir(Some(dir.join("renders")));
        let export = |s: &mut Session, name: &str| {
            let out = dir.join(name);
            let job = s.export(ExportSettings {
                path: out.clone(),
                sample_rate: 48_000,
                bits: 32,
                tail_seconds: 0.0,
            });
            loop {
                if let Some(r) = job.take_result() {
                    r.expect("export");
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            peel_media::decode_file(&out).unwrap().left().to_vec()
        };
        let before = export(&mut s, "before.wav");
        let bass = s
            .project()
            .arrangement()
            .clips
            .iter()
            .find(|c| c.start == Time::beats(16))
            .unwrap()
            .clone();
        let lanes_before = s.project().arrangement().tracks.len();
        s.render_clip_to_audio(bass.id).expect("start");
        let start = Instant::now();
        while s.render_progress().is_some() {
            assert!(start.elapsed().as_secs() < 30, "render finished");
            s.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let arr = s.project().arrangement();
        assert_eq!(arr.tracks.len(), lanes_before + 1);
        assert!(
            arr.clips.iter().find(|c| c.id == bass.id).unwrap().muted,
            "the pattern clip is kept, muted"
        );
        let lane_at = arr.tracks.iter().position(|t| t.id == bass.track).unwrap();
        let audio = arr
            .clips
            .iter()
            .find(|c| matches!(c.content, ClipContent::Audio { .. }))
            .expect("audio clip")
            .clone();
        assert_eq!(audio.track, arr.tracks[lane_at + 1].id, "on the lane below");
        assert_eq!(audio.start, bass.start);
        assert_eq!(s.doc.undo_label(), Some("Render to audio"));
        // Wait for the new audio to decode, then the song sounds the same.
        let ClipContent::Audio { asset, .. } = audio.content else {
            unreachable!()
        };
        while s.sample(asset).is_none() {
            assert!(start.elapsed().as_secs() < 30, "decoded");
            s.tick();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let after = export(&mut s, "after.wav");
        let rms = |x: &[f32]| (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt();
        let beat = 48_000.0 * 60.0 / 118.0;
        let (a, b) = ((17.0 * beat) as usize, (31.0 * beat) as usize);
        let (r0, r1) = (rms(&before[a..b]), rms(&after[a..b]));
        assert!((20.0 * (r1 / r0).log10()).abs() < 0.5, "{r0} vs {r1}");
        let diff: Vec<f32> = before[a..b]
            .iter()
            .zip(&after[a..b])
            .map(|(x, y)| x - y)
            .collect();
        assert!(
            rms(&diff) < r0 * 0.1,
            "nearly identical: residue {} of {r0}",
            rms(&diff)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_loop_following_the_tempo_lands_on_the_song_beats() {
        use peel_document::{ClipContent, Time};
        let dir = std::env::temp_dir().join(format!("peel-stretch-test-{}", Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        // Two bars at 100 BPM: a click on every beat.
        let sr = 48_000u32;
        let beat = 0.6f64;
        let mut x = vec![0.0f32; (8.0 * beat * sr as f64) as usize];
        for k in 0..8 {
            let at = (k as f64 * beat * sr as f64) as usize;
            for i in 0..240 {
                x[at + i] = (1.0 - i as f32 / 240.0) * if i % 2 == 0 { 0.8 } else { -0.8 };
            }
        }
        let wav = dir.join("loop.wav");
        peel_media::write_wav(&wav, &[&x, &x], sr, peel_media::WavFormat::FLOAT32).unwrap();
        let mut p = template::empty();
        p.set_bpm(118.0);
        let mut s = Session::new_silent(p);
        let lane = s.project().arrangement().tracks[0].id;
        let clip = s.add_audio_clip(lane, Time::ZERO, &wav).expect("clip");
        s.doc.edit("Follow", |p| {
            let c = p
                .arrangement_mut()
                .clips
                .iter_mut()
                .find(|c| c.id == clip)
                .unwrap();
            c.length = Time::beats(8);
            if let ClipContent::Audio { source_bpm, .. } = &mut c.content {
                *source_bpm = Some(100.0);
            }
        });
        let start = Instant::now();
        let out = dir.join("out.wav");
        let job = loop {
            s.tick();
            let asset = match &s.project().arrangement().clips[0].content {
                ClipContent::Audio { asset, .. } => *asset,
                _ => unreachable!(),
            };
            if s.sample(asset).is_some() {
                break s.export(ExportSettings {
                    path: out.clone(),
                    sample_rate: sr,
                    bits: 32,
                    tail_seconds: 0.0,
                });
            }
            assert!(start.elapsed().as_secs() < 20);
            std::thread::sleep(std::time::Duration::from_millis(5));
        };
        loop {
            if let Some(r) = job.take_result() {
                r.expect("export");
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let y = peel_media::decode_file(&out).unwrap().left().to_vec();
        let song_beat = 60.0 / 118.0;
        assert!(
            (y.len() as f64 / sr as f64 - 8.0 * song_beat).abs() < 0.05,
            "eight song beats long"
        );
        for k in 1..8 {
            let at = (k as f64 * song_beat * sr as f64) as usize;
            let win = &y[at.saturating_sub(600)..(at + 600).min(y.len())];
            assert!(
                win.iter().any(|v| v.abs() > 0.4),
                "click {k} on song beat {k}"
            );
            // Halfway between beats is quiet.
            let mid = at - (song_beat * 0.5 * sr as f64) as usize;
            let gap = &y[mid - 300..mid + 300];
            assert!(
                gap.iter().all(|v| v.abs() < 0.1),
                "silence between clicks before beat {k}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_sidechain_that_closes_a_loop_is_rejected() {
        let mut project = template::beat_and_melody();
        let master = project.mixer.tracks[0].id;
        let drums = project
            .channels
            .iter()
            .find(|c| c.name == "Kick")
            .unwrap()
            .track;
        // The kick track feeds the master; keying the kick from the master
        // would make each wait for the other.
        let mut comp = template::device("peel.fx.compressor", None);
        comp.sidechain = Some(master);
        project.mixer.track_mut(drums).unwrap().inserts.push(comp);
        assert!(
            peel_document::validate(&project)
                .iter()
                .any(|i| i.is_error())
        );
    }

    #[test]
    fn curved_automation_compiles_to_the_same_curve() {
        use peel_document::{AutomationPoint, AutomationTarget, Clip, ClipContent, Time};
        let mut project = template::beat_and_melody();
        let track = project.mixer.tracks[1].id;
        let lane = project.arrangement().tracks[5].id;
        let mut a = AutomationPoint::new(Time::ZERO, 0.1);
        a.tension = 0.6;
        let points = vec![a, AutomationPoint::new(Time::beats(8), 0.9)];
        project.arrangement_mut().clips.push(Clip {
            id: Id::new(),
            track: lane,
            start: Time::beats(4),
            length: Time::beats(8),
            offset: Time::ZERO,
            muted: false,
            content: ClipContent::Automation {
                target: AutomationTarget::TrackPan { track },
                points: points.clone(),
            },
        });
        let s = Session::new_silent(project);
        let p = s.project();
        let slots = HashMap::new();
        let meters: HashMap<Id, u16> = p
            .mixer
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id, i as u16))
            .collect();
        let samples = HashMap::new();
        let ctx = CompileContext {
            mode: PlayMode::Song,
            slots: &slots,
            meters: &meters,
            samples: &samples,
        };
        let (plan, _) = compile(p, &ctx);
        let lane = plan.automation.first().expect("lane");
        assert!(lane.points.len() > 2, "curve subdivided");
        for beat in [5.0, 6.0, 8.0, 10.0, 11.5] {
            let want = peel_document::automation_value(&points, Time::from_f64(beat - 4.0, 960))
                .unwrap()
                * 2.0
                - 1.0;
            let got = lane.value_at(beat).unwrap();
            assert!((got - want).abs() < 0.02, "beat {beat}: {got} vs {want}");
        }
    }

    #[test]
    fn exported_volume_automation_fades_the_song_out() {
        use peel_document::{AutomationPoint, AutomationTarget, Clip, ClipContent, Time};
        let dir = std::env::temp_dir().join(format!("peel-auto-test-{}", Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("fade.wav");
        let mut project = template::beat_and_melody();
        let master = project.mixer.tracks[0].id;
        let lane = project.arrangement().tracks[5].id;
        let unity = peel_document::db_to_fader(0.0);
        project.arrangement_mut().clips.push(Clip {
            id: Id::new(),
            track: lane,
            start: Time::ZERO,
            length: Time::beats(32),
            offset: Time::ZERO,
            muted: false,
            content: ClipContent::Automation {
                target: AutomationTarget::TrackGain { track: master },
                points: vec![
                    AutomationPoint {
                        position: Time::beats(16),
                        value: unity,
                        tension: 0.0,
                    },
                    AutomationPoint {
                        position: Time::beats(28),
                        value: 0.0,
                        tension: 0.0,
                    },
                ],
            },
        });
        assert!(
            peel_document::validate(&project)
                .iter()
                .all(|i| !i.is_error())
        );
        let settings = ExportSettings {
            path: out.clone(),
            sample_rate: 44_100,
            bits: 24,
            tail_seconds: 0.0,
        };
        let progress = std::sync::atomic::AtomicU32::new(0);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        render_to_wav(&project, &HashMap::new(), &settings, &progress, &cancel).expect("render");
        let buf = peel_media::decode_file(&out).expect("decode");
        let sec = |beat: f64| (beat * 60.0 / 118.0 * 44_100.0) as usize;
        let rms = |a: f64, b: f64| {
            let s = &buf.left()[sec(a)..sec(b)];
            (s.iter().map(|x| x * x).sum::<f32>() / s.len() as f32).sqrt()
        };
        let before = rms(8.0, 16.0);
        let after = rms(28.5, 31.5);
        assert!(before > 0.05, "playing before the fade: {before}");
        assert!(
            after < before * 0.01,
            "silent after the fade: {after} vs {before}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn export_renders_audio() {
        let dir = std::env::temp_dir().join(format!("peel-export-test-{}", Id::new()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = dir.join("mix.wav");
        let project = template::beat_and_melody();
        let settings = ExportSettings {
            path: out.clone(),
            sample_rate: 44_100,
            bits: 16,
            tail_seconds: 1.0,
        };
        let progress = std::sync::atomic::AtomicU32::new(0);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        render_to_wav(&project, &HashMap::new(), &settings, &progress, &cancel).expect("render");
        let buf = peel_media::decode_file(&out).expect("decode");
        assert_eq!(buf.sample_rate, 44_100.0);
        let secs = buf.duration_seconds();
        let expected = 32.0 * 60.0 / 118.0 + 1.0;
        assert!((secs - expected).abs() < 0.05, "{secs} vs {expected}");
        let peak = buf.left().iter().fold(0.0f32, |m, x| m.max(x.abs()));
        assert!(peak > 0.1 && peak <= 1.0, "peak {peak}");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[cfg(test)]
mod sync_tests;

#[cfg(test)]
mod meter_tests;
