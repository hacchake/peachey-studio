use peel_session::editing::check_times;
pub(crate) use peel_session::editing::time;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use peel_document::{
    Channel, Clip, ClipContent, DeviceInstance, Id, MixerTrack, Note, Pattern, Project, Time,
};
use peel_dsp::DeviceKind;
use peel_session::{AudioExportSettings, ExportJob, PlayMode, Session};
use serde_json::{Value, json};

use crate::actions::*;

#[derive(Default)]
pub struct Control {
    next_job: u64,
    jobs: BTreeMap<u64, Job>,
}

struct Job {
    running: Option<ExportJob>,
    result: Option<Result<PathBuf, String>>,
}

impl Control {
    pub fn tick(&mut self) {
        for job in self.jobs.values_mut() {
            if let Some(running) = &job.running {
                running.idle();
                if let Some(result) = running.take_result() {
                    job.result = Some(result);
                    job.running = None;
                }
            }
        }
    }

    /// Stop sound and background work when the MCP connection ends.
    pub fn disconnect(&mut self, session: &mut Session) {
        for job in self.jobs.values() {
            if let Some(running) = &job.running {
                running.cancel();
            }
        }
        session.cancel_recording();
        session.set_monitoring(false, 0.0);
        session.close_input();
        session.stop();
        session.stop_performance();
        session.stop_preview();
        session.panic();
    }

    pub fn execute(&mut self, s: &mut Session, action: Action) -> Result<Value, String> {
        self.tick();
        match action {
            Action::Help {} => Ok(json!({"guide": include_str!("../../../docs/MCP.md")})),
            Action::Status {} => Ok(json!({
                "project_id": s.project().id, "title": s.project().meta.title,
                "revision": s.doc.revision(), "dirty": s.doc.is_dirty(), "path": s.path,
                "playing": s.is_playing(), "beat": s.position(),
                "mode": if s.mode() == PlayMode::Song { "song" } else { "pattern" },
                "selected_pattern": s.selected_pattern(), "looping": s.looping(), "metronome": s.metronome(),
                "audio_device": s.audio_device_name(), "sample_rate": s.sample_rate(),
                "audio_healthy": s.audio_is_healthy(), "audio_warning": s.audio_warning(),
                "compensation_latency_samples": s.compensation_latency_samples(),
                "compensation_error": s.compensation_error(),
                "compensation_latency_ms": s.compensation_latency_samples() as f32 / s.sample_rate().max(1.0) * 1000.0,
                "assets": s.project().assets.iter().map(|a| json!({"id":a.id,"loading":s.audio_loading(a.id),"duration_seconds":s.asset_duration_seconds(a.id)})).collect::<Vec<_>>(),
                "missing_devices": s.missing_devices(), "plugins_scanning": s.plugins_scanning(),
                "recording": s.is_recording(), "pattern_recording":s.pattern_recording_active(), "armed_lanes": s.armed_lanes(),
                "record_blocker": s.record_blocker(), "input_open": s.input_is_open(),
                "input_error": s.input_error(), "input_warning": s.input_warning(),
                "monitoring": s.monitoring(), "midi_enabled": s.midi_enabled(), "mpe": s.mpe_config(),
                "performing": s.performing(), "capturing": s.performance_capturing(),
                "render_progress": s.render_progress(), "stretching": s.stretching(),
                "undo": s.doc.history_labels().collect::<Vec<_>>(),
                "redo": s.doc.redo_labels().collect::<Vec<_>>(),
                "export_jobs": self.jobs.keys().collect::<Vec<_>>()
            })),
            Action::ReadProject { pointer } => {
                let doc = serde_json::to_value(s.project()).map_err(err)?;
                let pointer = pointer.unwrap_or_default();
                let value = doc.pointer(&pointer).ok_or("JSON Pointer does not exist")?;
                Ok(
                    json!({"project_id": s.project().id, "revision": s.doc.revision(), "value": value}),
                )
            }
            Action::RescanPlugins {} => {
                s.rescan_plugins();
                Ok(json!({"scanning":s.plugins_scanning()}))
            }
            Action::PluginWindow { device, open } => {
                let device_id = id(&device)?;
                if open.unwrap_or(true) {
                    s.open_plugin_editor(device_id)?;
                } else {
                    s.close_plugin_editor(device_id);
                }
                Ok(json!({"open": s.plugin_editor_open(device_id)}))
            }
            Action::BrowseFiles {
                directory,
                offset,
                limit,
            } => {
                let limit = limit.unwrap_or(100);
                if !(1..=1000).contains(&limit) {
                    return Err("limit must be 1..=1000".into());
                }
                let directory = absolute(&directory)?;
                let mut entries = Vec::new();
                for entry in std::fs::read_dir(directory).map_err(err)? {
                    let entry = entry.map_err(err)?;
                    let path = entry.path();
                    let is_dir = path.is_dir();
                    let extension = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if is_dir
                        || peel_media::is_audio_file(&path)
                        || matches!(extension.as_str(), "peelproj" | "mid" | "midi" | "rmi")
                    {
                        entries.push((
                            !is_dir,
                            entry.file_name().to_string_lossy().into_owned(),
                            path,
                        ));
                    }
                }
                entries.sort();
                let total = entries.len();
                let offset = offset.unwrap_or(0);
                let files = entries
                    .into_iter()
                    .skip(offset)
                    .take(limit)
                    .map(|(file, name, path)| json!({"name":name,"path":path,"directory":!file}))
                    .collect::<Vec<_>>();
                let next_offset = (offset.saturating_add(files.len()) < total)
                    .then_some(offset.saturating_add(files.len()));
                Ok(json!({"files":files,"total":total,"next_offset":next_offset}))
            }
            Action::NoteExpression {
                pattern,
                note,
                pitch,
                pressure,
                timbre,
            } => crate::expression_control::note_expression(
                s, &pattern, &note, pitch, pressure, timbre,
            ),
            Action::TransformNotes {
                pattern,
                selection,
                tool,
            } => {
                let mut p = s.project().clone();
                let label = crate::compose::transform(&mut p, id(&pattern)?, selection, tool)?;
                commit(s, label, p)
            }
            Action::NewIds { count } => {
                if !(1..=256).contains(&count) {
                    return Err("count must be 1..=256".into());
                }
                Ok(json!({"ids": (0..count).map(|_| Id::new()).collect::<Vec<_>>()}))
            }
            Action::Devices { type_id } => devices(s, type_id.as_deref()),
            Action::NewProject {
                title,
                demo,
                song,
                discard_changes,
            } => {
                can_replace(s, discard_changes)?;
                let p = match song {
                    Some(name) => built_in_song(&name)?,
                    None if demo => peel_session::template::showcase(),
                    None => Project::empty(title),
                };
                s.replace_project(p);
                Ok(json!({"project_id": s.project().id}))
            }
            Action::OpenProject {
                path,
                discard_changes,
            } => {
                can_replace(s, discard_changes)?;
                s.open(&absolute(&path)?)?;
                Ok(json!({"project_id": s.project().id, "path": s.path}))
            }
            Action::SaveProject { path } => {
                Ok(json!({"path": s.save(path.as_deref().map(absolute).transpose()?)?}))
            }
            Action::EditProject {
                expected_project_id,
                expected_revision,
                label,
                operations,
            } => {
                if id(&expected_project_id)? != s.project().id {
                    return Err("The active project changed. Read it again before editing.".into());
                }
                if expected_revision != s.doc.revision() {
                    return Err(format!(
                        "Stale revision: expected {expected_revision}, current {}. Read the project again before editing.",
                        s.doc.revision()
                    ));
                }
                if operations.0.len() > 4096 {
                    return Err("At most 4096 patch operations per transaction".into());
                }
                let mut value = serde_json::to_value(s.project()).map_err(err)?;
                json_patch::patch(&mut value, &operations).map_err(err)?;
                check_times(&value, None)?;
                let p: Project = serde_json::from_value(value).map_err(err)?;
                if p.id != s.project().id
                    || p.format != s.project().format
                    || p.schema != s.project().schema
                    || p.revision != s.project().revision
                {
                    return Err(
                        "Project identity, format, schema and revision cannot be patched".into(),
                    );
                }
                commit(s, &label, p)
            }
            Action::History { direction } => {
                if s.is_recording() {
                    return Err("Stop recording before changing history".into());
                }
                s.doc.end_gesture();
                let label = match direction {
                    HistoryDirection::Undo => s.doc.undo(),
                    HistoryDirection::Redo => s.doc.redo(),
                };
                Ok(json!({"label": label, "revision": s.doc.revision()}))
            }
            Action::AddChannel { name, type_id } => {
                let version = device_version(s, &type_id, DeviceKind::Instrument)?;
                let root_key = match type_id.as_str() {
                    "peel.drum.acoustic" => 36,
                    "peel.guitar.recorded" | "peel.guitar.palm_muted" => 40,
                    "peel.bass.recorded" => 28,
                    _ => 60,
                };
                let mut p = s.project().clone();
                let color = p.next_channel_color();
                let track = MixerTrack {
                    id: Id::new(),
                    name: name.clone(),
                    color,
                    gain_db: 0.0,
                    pan: 0.0,
                    muted: false,
                    solo: false,
                    inserts: vec![],
                    output: None,
                    sends: vec![],
                };
                let channel = Channel {
                    id: Id::new(),
                    name,
                    color,
                    device: DeviceInstance::new(type_id, version),
                    track: track.id,
                    gain_db: 0.0,
                    pan: 0.0,
                    muted: false,
                    root_key,
                };
                let result =
                    json!({"channel": channel.id, "device": channel.device.id, "track": track.id});
                p.mixer.tracks.push(track);
                p.channels.push(channel);
                commit(s, "Add channel", p)?;
                Ok(result)
            }
            Action::AddPattern { name, length } => {
                let mut p = s.project().clone();
                let pattern = Pattern {
                    expression: Vec::new(),
                    id: Id::new(),
                    name,
                    color: p.next_channel_color(),
                    length: time(&length)?,
                    notes: vec![],
                };
                let id = pattern.id;
                p.patterns.push(Arc::new(pattern));
                commit(s, "Add pattern", p)?;
                Ok(json!({"pattern": id}))
            }
            Action::SetNotes {
                pattern,
                channel,
                notes,
            } => {
                let (pattern, channel) = (id(&pattern)?, id(&channel)?);
                if s.project().channel(channel).is_none() {
                    return Err("Channel does not exist".into());
                }
                if notes.len() > 100_000 {
                    return Err("At most 100000 notes per call".into());
                }
                let mut p = s.project().clone();
                let pattern = p.pattern_mut(pattern).ok_or("Pattern does not exist")?;
                let notes = notes
                    .into_iter()
                    .map(|n| {
                        Ok(Note::new(
                            channel,
                            time(&n.start)?,
                            time(&n.length)?,
                            n.key,
                            n.velocity,
                        ))
                    })
                    .collect::<Result<Vec<_>, String>>()?;
                let ids: Vec<_> = notes.iter().map(|n| n.id).collect();
                pattern.notes.retain(|n| n.channel != channel);
                pattern.notes.extend(notes);
                pattern.retain_expression();
                pattern.sort_notes();
                commit(s, "Set notes", p)?;
                Ok(json!({"notes": ids, "revision": s.doc.revision()}))
            }
            Action::PlacePattern {
                pattern,
                lane,
                start,
                length,
            } => {
                let mut p = s.project().clone();
                let clip = Clip {
                    id: Id::new(),
                    track: id(&lane)?,
                    start: time(&start)?,
                    length: time(&length)?,
                    offset: Time::ZERO,
                    muted: false,
                    content: ClipContent::Pattern {
                        pattern: id(&pattern)?,
                    },
                };
                let id = clip.id;
                p.arrangement_mut().clips.push(clip);
                commit(s, "Place pattern", p)?;
                Ok(json!({"clip": id}))
            }
            Action::AddEffect { track, type_id } => {
                let version = device_version(s, &type_id, DeviceKind::Effect)?;
                let mut p = s.project().clone();
                let instance = DeviceInstance::new(type_id, version);
                let instance_id = instance.id;
                p.mixer
                    .track_mut(id(&track)?)
                    .ok_or("Mixer track does not exist")?
                    .inserts
                    .push(instance);
                commit(s, "Add effect", p)?;
                Ok(json!({"device": instance_id}))
            }
            Action::SetDevice {
                device,
                preset,
                params,
            } => {
                let mut p = s.project().clone();
                let device_id = id(&device)?;
                let d = peel_session::find_device_mut(&mut p, device_id)
                    .ok_or("Device does not exist")?;
                let old_piece = d.params.get("kit.piece").copied().unwrap_or(0.0);
                if let Some(name) = preset {
                    let preset = peel_devices::presets(&d.type_id)
                        .iter()
                        .find(|p| p.name == name)
                        .ok_or("Factory preset does not exist")?;
                    let desc = peel_devices::descriptor(&d.type_id)
                        .ok_or("Device descriptor unavailable")?;
                    d.params = desc
                        .params
                        .iter()
                        .map(|p| (p.key.to_string(), p.default))
                        .collect();
                    d.params
                        .extend(preset.params.iter().map(|(k, v)| (k.to_string(), *v)));
                    d.preset = Some(name);
                }
                d.params.extend(params);
                peel_session::update_recorded_kit_mapping(&mut p, device_id, old_piece);
                commit(s, "Set device", p)
            }
            Action::Transport {
                command,
                mode,
                pattern,
                beat,
                looping,
                metronome,
            } => {
                // Resolve every argument before changing any live state.
                let pattern = pattern.as_deref().map(id).transpose()?;
                if pattern.is_some_and(|id| s.project().pattern(id).is_none()) {
                    return Err("Pattern does not exist".into());
                }
                if beat.is_some_and(|b| !b.is_finite() || !(0.0..=10_000_000.0).contains(&b)) {
                    return Err("beat must be finite and between 0 and 10000000".into());
                }
                if let Some(p) = pattern {
                    s.select_pattern(p);
                }
                if let Some(mode) = mode {
                    s.set_mode(match mode {
                        Mode::Song => PlayMode::Song,
                        Mode::Pattern => PlayMode::Pattern(s.selected_pattern()),
                    });
                }
                if let Some(beat) = beat {
                    s.seek(beat);
                }
                if let Some(on) = looping {
                    s.set_looping(on);
                }
                if let Some(on) = metronome {
                    s.set_metronome(on);
                }
                match command {
                    Some(TransportCommand::Play) => s.play(),
                    Some(TransportCommand::Stop) => s.stop(),
                    Some(TransportCommand::Panic) => s.panic(),
                    None => {}
                }
                Ok(json!({"playing": s.is_playing(), "beat": s.position()}))
            }
            Action::LiveNote {
                channel,
                key,
                velocity,
                on,
            } => {
                let channel = id(&channel)?;
                if s.project().channel(channel).is_none()
                    || key > 127
                    || !(0.0..=1.0).contains(&velocity)
                {
                    return Err("Invalid channel, MIDI key or velocity".into());
                }
                s.live_note(channel, key, velocity, on);
                Ok(json!({"on": on}))
            }
            Action::ImportAudio { path, lane, start } => {
                let lane = id(&lane)?;
                lane_exists(s, lane)?;
                let start = time(&start)?;
                if start.is_negative() {
                    return Err("start must be nonnegative".into());
                }
                let clip = s.add_audio_clip(lane, start, &absolute(&path)?)?;
                Ok(json!({"clip": clip, "revision": s.doc.revision()}))
            }
            Action::PreviewAudio { path } => {
                if let Some(path) = path {
                    let path = absolute(&path)?;
                    if !path.is_file() {
                        return Err("Audio file does not exist".into());
                    }
                    s.preview_file(&path);
                } else {
                    s.stop_preview();
                }
                Ok(json!({}))
            }
            Action::BounceClip { clip } => {
                s.render_clip_to_audio(id(&clip)?)?;
                Ok(json!({"started": true}))
            }
            Action::ImportMidi { path, adopt_tempo } => {
                let bytes = std::fs::read(absolute(&path)?).map_err(err)?;
                let midi = peel_midi::import_smf(&bytes, &Default::default()).map_err(err)?;
                let warnings = midi.report.warnings.clone();
                let mut p = s.project().clone();
                let options = peel_midi::MergeOptions {
                    adopt_tempo,
                    ..Default::default()
                };
                midi.merge_with_new_mixer_tracks(&mut p, &options)
                    .map_err(err)?;
                commit(s, "Import MIDI", p)?;
                Ok(json!({"revision": s.doc.revision(), "warnings": warnings}))
            }
            Action::ExportMidi { path, pattern } => {
                let path = absolute(&path)?;
                let export = if let Some(pattern) = pattern {
                    peel_midi::export_pattern(s.project(), id(&pattern)?, &Default::default())
                } else {
                    peel_midi::export_arrangement(s.project(), &Default::default())
                }
                .map_err(err)?;
                std::fs::write(&path, export.bytes).map_err(err)?;
                Ok(
                    json!({"path": path, "warnings": export.report.warnings, "notes": export.report.notes}),
                )
            }
            Action::ExportAudio {
                path,
                stems,
                sample_rate,
                bits,
                bitrate_kbps,
                tail_seconds,
                mono,
                normalize,
                dither,
            } => {
                let mut settings = AudioExportSettings::default();
                settings.render.path = absolute(&path)?;
                settings.format =
                    peel_media::AudioFormat::from_extension(&settings.render.path).map_err(err)?;
                if let Some(v) = sample_rate {
                    settings.render.sample_rate = v;
                }
                if let Some(v) = bits {
                    settings.render.bits = v;
                }
                if let Some(v) = bitrate_kbps {
                    settings.bitrate_kbps = v;
                }
                if let Some(v) = tail_seconds {
                    settings.render.tail_seconds = v;
                }
                if let Some(v) = dither {
                    settings.dither = v;
                }
                settings.mono = mono;
                settings.normalize = normalize;
                settings.validate()?;
                if self.jobs.values().any(|j| j.running.is_some()) {
                    return Err("An MCP export is already running".into());
                }
                if self.jobs.len() >= 256 {
                    return Err("Export job limit reached; start a new MCP session".into());
                }
                let job = if stems {
                    s.export_stems(settings)
                } else {
                    s.export(settings)
                };
                self.next_job += 1;
                self.jobs.insert(
                    self.next_job,
                    Job {
                        running: Some(job),
                        result: None,
                    },
                );
                Ok(json!({"job": self.next_job}))
            }
            Action::ExportJob { job, cancel } => {
                let job = self.jobs.get(&job).ok_or("Export job does not exist")?;
                if cancel && let Some(running) = &job.running {
                    running.cancel();
                }
                Ok(
                    json!({"progress": job.running.as_ref().map_or(1.0, |j| j.progress()), "done": job.result.is_some(), "path": job.result.as_ref().and_then(|r| r.as_ref().ok()), "error": job.result.as_ref().and_then(|r| r.as_ref().err())}),
                )
            }
            Action::AudioDevices {} => Ok(json!({
                "outputs": s.audio_devices().into_iter().map(|d| json!({"id":d.id,"name":d.name,"default":d.is_default,"sample_rates":d.sample_rates,"buffer_frames":d.buffer_frames})).collect::<Vec<_>>(),
                "inputs": s.input_devices().into_iter().map(|d| json!({"id":d.id,"name":d.name,"default":d.is_default,"channels":d.max_channels,"sample_rates":d.sample_rates})).collect::<Vec<_>>(),
                "midi": s.midi_available_inputs().into_iter().map(|d| json!({"id":d.id,"name":d.name})).collect::<Vec<_>>()
            })),
            Action::ConfigureAudio {
                device,
                buffer_frames,
            } => {
                s.restart_audio(device, buffer_frames)?;
                Ok(json!({"device": s.audio_device_name()}))
            }
            Action::ConfigureMidi { port, channel } => {
                let channel = channel.as_deref().map(id).transpose()?;
                if channel.is_some_and(|ch| s.project().channel(ch).is_none()) {
                    return Err("Channel does not exist".into());
                }
                if let Some(port) = port {
                    if !s.midi_available_inputs().iter().any(|p| p.id == port) {
                        return Err("MIDI port does not exist".into());
                    }
                    s.select_midi_input(peel_session::PortSelection::Only(port));
                    s.set_midi_enabled(true);
                } else {
                    s.set_midi_enabled(false);
                }
                if channel.is_some() {
                    s.set_live_target(channel);
                }
                Ok(json!({"enabled": s.midi_enabled(), "channel": s.live_target()}))
            }
            Action::OutputRoutes {} => Ok(crate::io_control::output_routes(s)),
            Action::ConfigureOutputRoutes { routes } => {
                crate::io_control::configure_routes(s, routes)?;
                Ok(crate::io_control::output_routes(s))
            }
            Action::ConfigureOutputChannels { left, right } => {
                s.set_primary_output_channels(
                    right.map_or(peel_session::OutputChannels::Mono(left), |r| {
                        peel_session::OutputChannels::Stereo(left, r)
                    }),
                )?;
                Ok(crate::io_control::output_routes(s))
            }
            Action::MidiControls {} => Ok(crate::io_control::midi_controls(s)),
            Action::ConfigureMidiControls {
                channel_filter,
                mappings,
            } => {
                crate::io_control::configure_midi(s, channel_filter, mappings)?;
                Ok(crate::io_control::midi_controls(s))
            }
            Action::MidiLearn { target } => {
                if let Some(target) = target {
                    s.begin_midi_learn(crate::io_control::midi_target(target)?)?;
                } else {
                    s.cancel_midi_learn();
                }
                Ok(crate::io_control::midi_controls(s))
            }
            Action::MidiControl {
                channel,
                controller,
                value,
            } => {
                crate::io_control::send_control(s, channel, controller, value)?;
                Ok(crate::io_control::midi_controls(s))
            }
            Action::Mpe {
                mode,
                members,
                member_bend_range,
                manager_bend_range,
            } => crate::expression_control::mpe(
                s,
                mode,
                members,
                member_bend_range,
                manager_bend_range,
            ),
            Action::MidiInput { channel, event } => {
                crate::expression_control::midi_input(s, channel, event)
            }
            Action::MidiExpression {
                bend,
                pressure,
                key_pressure,
                bend_range,
                reset,
            } => {
                crate::io_control::send_expression(
                    s,
                    bend,
                    pressure,
                    key_pressure,
                    bend_range,
                    reset,
                )?;
                Ok(crate::io_control::expression(s))
            }
            Action::ConfigureRecording {
                device,
                left,
                right,
                monitoring,
                monitor_gain,
                offset_ms,
                directory,
            } => {
                let gain = monitor_gain.unwrap_or(1.0);
                if !gain.is_finite() || !(0.0..=4.0).contains(&gain) {
                    return Err("monitor_gain must be 0..=4".into());
                }
                if offset_ms.is_some_and(|v| !v.is_finite() || v.abs() > 2000.0) {
                    return Err("offset_ms must be -2000..=2000".into());
                }
                let directory = directory.as_deref().map(absolute).transpose()?;
                if s.is_recording() {
                    return Err("Stop recording before configuring input".into());
                }
                s.set_input_device(device)?;
                s.set_input_channels(right.map_or(peel_session::InputChannels::Mono(left), |r| {
                    peel_session::InputChannels::Stereo(left, r)
                }));
                if let Some(v) = offset_ms {
                    s.set_record_offset_ms(v);
                }
                if let Some(dir) = directory {
                    s.set_recordings_dir(Some(dir));
                }
                s.set_monitoring(monitoring, gain);
                Ok(
                    json!({"input_open": s.input_is_open(), "warning": s.input_warning(), "error": s.input_error()}),
                )
            }
            Action::CloseInput {} => {
                if s.is_recording() {
                    return Err("Stop recording before closing input".into());
                }
                s.set_monitoring(false, 0.0);
                s.close_input();
                Ok(json!({"input_open":s.input_is_open()}))
            }
            Action::ArmLane { lane, armed } => {
                let lane = id(&lane)?;
                lane_exists(s, lane)?;
                s.arm_lane(lane, armed);
                Ok(json!({"armed_lanes": s.armed_lanes()}))
            }
            Action::Recording { command } => {
                let clips = match command {
                    RecordingCommand::Start => {
                        s.start_recording()?;
                        vec![]
                    }
                    RecordingCommand::Stop => s.stop_recording(),
                    RecordingCommand::Cancel => {
                        s.cancel_recording();
                        vec![]
                    }
                };
                Ok(json!({"recording": s.is_recording(), "clips": clips}))
            }
            Action::PerformanceSlots {} => Ok(
                json!({"lanes": s.performance_slots().into_iter().map(|(lane, slots)| json!({"lane":lane,"slots":slots.into_iter().map(|slot| json!({"clip":slot.clip,"name":slot.name,"state":format!("{:?}",s.slot_state(lane,slot.clip)).to_lowercase()})).collect::<Vec<_>>()})).collect::<Vec<_>>()}),
            ),
            Action::Performance {
                command,
                lane,
                clip,
                scene,
                capture,
            } => {
                match command {
                    PerformanceCommand::Launch => {
                        let lane = id(lane.as_deref().ok_or("lane is required")?)?;
                        let clip = id(clip.as_deref().ok_or("clip is required")?)?;
                        if !s
                            .performance_slots()
                            .iter()
                            .any(|(l, slots)| *l == lane && slots.iter().any(|sl| sl.clip == clip))
                        {
                            return Err("Launchable clip does not exist on this lane".into());
                        }
                        s.launch(lane, clip);
                    }
                    PerformanceCommand::Scene => {
                        let index = scene.ok_or("scene is required")?;
                        if !s
                            .performance_slots()
                            .iter()
                            .any(|(_, slots)| index < slots.len())
                        {
                            return Err("Scene does not exist".into());
                        }
                        s.launch_scene(index);
                    }
                    PerformanceCommand::StopLane => {
                        let lane = id(lane.as_deref().ok_or("lane is required")?)?;
                        lane_exists(s, lane)?;
                        s.stop_lane(lane);
                    }
                    PerformanceCommand::Stop => s.stop_performance(),
                    PerformanceCommand::Capture => {
                        s.set_performance_capture(capture.ok_or("capture is required")?)
                    }
                }
                Ok(json!({"performing": s.performing(), "capturing": s.performance_capturing()}))
            }
        }
    }
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}
pub(crate) fn id(s: &str) -> Result<Id, String> {
    s.parse().map_err(err)
}
fn absolute(path: &str) -> Result<PathBuf, String> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err("Use an absolute filesystem path".into());
    }
    Ok(path.into())
}
fn lane_exists(s: &Session, lane: Id) -> Result<(), String> {
    if s.project()
        .arrangement()
        .tracks
        .iter()
        .any(|t| t.id == lane)
    {
        Ok(())
    } else {
        Err("Arrangement lane does not exist".into())
    }
}
/// A demo song by title or a song starter by name, ignoring case.
fn built_in_song(name: &str) -> Result<Project, String> {
    use peel_session::template;
    let demos = template::demos();
    if let Some(d) = demos.iter().find(|d| d.title.eq_ignore_ascii_case(name)) {
        return Ok((d.make)());
    }
    let templates = template::templates();
    if let Some((_, make)) = templates.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)) {
        return Ok(make());
    }
    let known: Vec<&str> = demos
        .iter()
        .map(|d| d.title)
        .chain(templates.iter().map(|(n, _)| *n))
        .collect();
    Err(format!(
        "No song called {name}. Choose one of: {}",
        known.join(", ")
    ))
}
fn can_replace(s: &Session, discard: bool) -> Result<(), String> {
    if s.is_recording() || s.pattern_recording_active() {
        return Err("Stop recording before replacing the project".into());
    }
    if s.doc.is_dirty() && !discard {
        return Err("Unsaved changes: save first, or set discard_changes to true".into());
    }
    Ok(())
}
fn device_version(s: &Session, type_id: &str, kind: DeviceKind) -> Result<u32, String> {
    if let Some(d) = peel_devices::descriptor(type_id) {
        if d.kind != kind {
            return Err(format!("Expected {kind:?}, got {:?}", d.kind));
        }
        return Ok(d.version);
    }
    // CLAP parameter descriptors exist only after the first instance loads.
    // The scanner's metadata is enough to create that first instance.
    if let Some(plugin) = s
        .plugins()
        .iter()
        .find(|p| peel_session::plugin_type_id(p) == type_id)
    {
        if !plugin.is_hostable() {
            return Err("Plugin cannot be hosted".into());
        }
        let matches = match kind {
            DeviceKind::Instrument => plugin.is_instrument(),
            DeviceKind::Effect => plugin.is_effect(),
        };
        if matches {
            return Ok(1);
        }
        return Err(format!("Plugin is not an {kind:?}"));
    }
    Err("Unknown device type; call peel_devices first".into())
}

fn devices(s: &Session, type_id: Option<&str>) -> Result<Value, String> {
    let mut descriptors: Vec<_> = peel_devices::all().collect();
    for plugin in s.plugins() {
        if let Some(d) = peel_devices::descriptor(&peel_session::plugin_type_id(plugin)) {
            descriptors.push(d);
        }
    }
    if let Some(type_id) = type_id {
        descriptors.retain(|d| d.type_id == type_id);
        if descriptors.is_empty() {
            if let Some(plugin) = s
                .plugins()
                .iter()
                .find(|p| peel_session::plugin_type_id(p) == type_id)
            {
                return Ok(
                    json!({"devices":[{"type_id":type_id,"name":plugin.name,"description":plugin.description,"kind":if plugin.is_instrument() {"instrument"} else {"effect"},"parameters_available":false,"instruction":"Add an instance, then call peel_devices again for its parameter schema"}],"plugins_scanning":s.plugins_scanning()}),
                );
            }
            return Err("Unknown device type (plugins may still be scanning)".into());
        }
    }
    Ok(json!({"devices": descriptors.into_iter().map(|d| {
        let mut v = json!({"type_id":d.type_id,"name":d.name,"description":d.description,"kind":format!("{:?}",d.kind).to_lowercase(),"category":d.category,"version":d.version});
        if type_id.is_some() {
            v["params"] = json!(d.params.iter().map(|p| json!({"key":p.key,"name":p.name,"min":p.min,"max":p.max,"default":p.default,"unit":format!("{:?}",p.unit),"choices":p.choices})).collect::<Vec<_>>());
            v["presets"] = json!(peel_devices::presets(d.type_id).iter().map(|p| p.name).collect::<Vec<_>>());
        }
        v
    }).collect::<Vec<_>>(),"plugins_scanning":s.plugins_scanning(),"plugins":s.plugins().iter().map(|p| json!({"type_id":peel_session::plugin_type_id(p),"name":p.name,"kind":if p.is_instrument() {"instrument"} else {"effect"}})).collect::<Vec<_>>()}))
}

impl Drop for Control {
    fn drop(&mut self) {
        for job in self.jobs.values() {
            if let Some(running) = &job.running {
                running.cancel();
            }
        }
    }
}

pub(crate) fn commit(s: &mut Session, label: &str, p: Project) -> Result<Value, String> {
    s.apply_project_edit(s.project().id, s.doc.revision(), label, p)?;
    let warnings = peel_document::validate(s.project())
        .into_iter()
        .filter(|i| !i.is_error())
        .map(|i| i.message)
        .collect::<Vec<_>>();
    Ok(json!({"revision": s.doc.revision(), "warnings": warnings}))
}
