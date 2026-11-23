//! Offline export. Renders an immutable snapshot of the project with fresh
//! device instances through the same engine code path as playback.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use peel_document::{ClipContent, Id, Project};
use peel_dsp::{Device, SampleBuffer};
use peel_engine::{EngineCommand, EngineConfig, OfflineEngine, Slot};
use peel_media::{AudioEncoding, AudioFormat};

use crate::compile::{CompileContext, PlayMode, compile};

#[derive(Clone, Debug)]
pub struct ExportSettings {
    pub path: PathBuf,
    pub sample_rate: u32,
    /// 16, 24 or 32 (float).
    pub bits: u16,
    /// Seconds rendered after the arrangement ends (reverb and delay tails).
    pub tail_seconds: f32,
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            path: PathBuf::new(),
            sample_rate: 48_000,
            bits: 24,
            tail_seconds: 3.0,
        }
    }
}

/// Format and processing options for an offline mix or stem export.
#[derive(Clone, Debug)]
pub struct AudioExportSettings {
    pub render: ExportSettings,
    pub format: AudioFormat,
    /// Constant MP3 bitrate; ignored by lossless formats.
    pub bitrate_kbps: u16,
    /// TPDF dither for integer PCM output.
    pub dither: bool,
    /// Downmix left and right at equal gain before encoding.
    pub mono: bool,
    /// Adjust the file's peak to -1 dBFS after downmixing. Silent files stay silent.
    pub normalize: bool,
}

impl Default for AudioExportSettings {
    fn default() -> Self {
        Self {
            render: ExportSettings::default(),
            format: AudioFormat::Wav,
            bitrate_kbps: 320,
            dither: true,
            mono: false,
            normalize: false,
        }
    }
}

impl From<ExportSettings> for AudioExportSettings {
    fn from(render: ExportSettings) -> Self {
        Self {
            render,
            ..Self::default()
        }
    }
}

impl AudioExportSettings {
    pub fn encoding(&self) -> AudioEncoding {
        AudioEncoding {
            format: self.format,
            bits: self.render.bits,
            bitrate_kbps: self.bitrate_kbps,
            dither: self.dither,
        }
    }

    /// Validate before constructing devices or allocating render buffers.
    pub fn validate(&self) -> Result<(), String> {
        self.encoding()
            .validate(self.render.sample_rate)
            .map_err(|e| e.to_string())?;
        if !self.render.tail_seconds.is_finite()
            || !(0.0..=60.0).contains(&self.render.tail_seconds)
        {
            return Err("Tail must be between 0 and 60 seconds".into());
        }
        if self.render.path.file_name().is_none() {
            return Err("Choose an export file".into());
        }
        let extension =
            AudioFormat::from_extension(&self.render.path).map_err(|e| e.to_string())?;
        if extension != self.format {
            return Err(format!(
                "{} exports need a .{} file extension",
                self.format.label(),
                self.format.extension()
            ));
        }
        Ok(())
    }
}

/// A running export; poll `progress()` and `result()` from the UI.
pub struct ExportJob {
    progress: Arc<AtomicU32>,
    cancel: Arc<AtomicBool>,
    result: Arc<Mutex<Option<Result<PathBuf, String>>>>,
    /// Plugin instances rendering in the job. Kept here so they are
    /// serviced from, and finally destroyed on, the main thread.
    plugins: Vec<crate::PluginHandle>,
}

/// Devices built on the main thread for an export (plugins), by instance id.
pub type Prebuilt = HashMap<Id, Box<dyn Device>>;

/// One file an export job writes.
pub(crate) struct Render {
    pub project: Project,
    pub prebuilt: Prebuilt,
    pub settings: AudioExportSettings,
}

/// Mixer tracks worth a stem: anything a channel, an audio clip or another
/// track plays into. Excludes the master.
pub fn stem_tracks(p: &Project) -> Vec<Id> {
    let master = p.mixer.tracks[0].id;
    let mut used: std::collections::HashSet<Id> = p.channels.iter().map(|c| c.track).collect();
    for a in &p.arrangements {
        for c in &a.clips {
            if let peel_document::ClipContent::Audio { track, .. } = &c.content {
                used.insert(*track);
            }
        }
    }
    for t in &p.mixer.tracks {
        used.extend(t.sends.iter().map(|s| s.target));
        if let Some(o) = t.output {
            used.insert(o);
        }
    }
    p.mixer
        .tracks
        .iter()
        .filter(|t| t.id != master && used.contains(&t.id))
        .map(|t| t.id)
        .collect()
}

/// A file name part made safe for every platform.
pub(crate) fn file_safe(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || " -_.()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "Track".into() } else { s }
}

impl ExportJob {
    pub(crate) fn failed(error: String) -> Self {
        Self {
            progress: Arc::new(AtomicU32::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            result: Arc::new(Mutex::new(Some(Err(error)))),
            plugins: Vec::new(),
        }
    }

    /// Starts an export. `loads` are audio files the clips need that the
    /// session has not decoded yet; the export thread decodes them itself.
    pub(crate) fn start(
        project: Project,
        samples: HashMap<Id, Arc<SampleBuffer>>,
        loads: Vec<(Id, PathBuf)>,
        prebuilt: Prebuilt,
        plugins: Vec<crate::PluginHandle>,
        settings: impl Into<AudioExportSettings>,
    ) -> ExportJob {
        let settings = settings.into();
        let rate = settings.render.sample_rate;
        let renders = vec![Render {
            project,
            prebuilt,
            settings,
        }];
        ExportJob::start_many(renders, samples, loads, plugins, rate, None)
    }

    /// Runs several renders in turn (stems), reporting combined progress.
    /// `done` is the result reported when every render succeeds (the stems
    /// folder); a single render reports its file.
    pub(crate) fn start_many(
        renders: Vec<Render>,
        samples: HashMap<Id, Arc<SampleBuffer>>,
        loads: Vec<(Id, PathBuf)>,
        plugins: Vec<crate::PluginHandle>,
        sample_rate: u32,
        done: Option<PathBuf>,
    ) -> ExportJob {
        let progress = Arc::new(AtomicU32::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let result = Arc::new(Mutex::new(None));
        let (p, c, r) = (progress.clone(), cancel.clone(), result.clone());
        std::thread::Builder::new()
            .name("peel-export".into())
            .spawn(move || {
                let mut samples = samples;
                for (id, path) in loads {
                    match peel_media::load_for_engine(&path, sample_rate as f32) {
                        Ok(buf) => {
                            samples.insert(id, Arc::new(buf));
                        }
                        Err(e) => log::warn!("export: could not load {}: {e}", path.display()),
                    }
                }
                let n = renders.len().max(1) as f32;
                let mut out = Err("Nothing to export".to_string());
                for (i, job) in renders.into_iter().enumerate() {
                    let report =
                        |f: f32| p.store(((i as f32 + f) / n).to_bits(), Ordering::Relaxed);
                    out = render_with(
                        &job.project,
                        &samples,
                        job.prebuilt,
                        &job.settings,
                        &report,
                        &c,
                    );
                    if out.is_err() {
                        break;
                    }
                }
                if let (Ok(_), Some(dir)) = (&out, &done) {
                    out = Ok(dir.clone());
                }
                if let Ok(mut slot) = r.lock() {
                    *slot = Some(out);
                }
            })
            .expect("export thread");
        ExportJob {
            progress,
            cancel,
            result,
            plugins,
        }
    }

    /// Services the job's plugins; call every frame while it runs.
    pub fn idle(&self) {
        for h in &self.plugins {
            let _ = h.idle();
        }
    }

    /// 0..=1.
    pub fn progress(&self) -> f32 {
        f32::from_bits(self.progress.load(Ordering::Relaxed))
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The finished result, once.
    pub fn take_result(&self) -> Option<Result<PathBuf, String>> {
        self.result.lock().ok()?.take()
    }
}

/// Renders the arrangement synchronously (used by the export thread and
/// the command-line tool).
pub fn render(
    project: &Project,
    samples: &HashMap<Id, Arc<SampleBuffer>>,
    settings: &ExportSettings,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    render_audio(project, samples, &settings.clone().into(), progress, cancel)
}

/// Renders to the selected audio format with optional downmixing and normalization.
pub fn render_audio(
    project: &Project,
    samples: &HashMap<Id, Arc<SampleBuffer>>,
    settings: &AudioExportSettings,
    progress: &AtomicU32,
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    let report = |f: f32| progress.store(f.to_bits(), Ordering::Relaxed);
    render_with(project, samples, Prebuilt::new(), settings, &report, cancel)
}

/// [`render`] with devices that had to be created on the main thread.
pub fn render_with(
    project: &Project,
    samples: &HashMap<Id, Arc<SampleBuffer>>,
    mut prebuilt: Prebuilt,
    options: &AudioExportSettings,
    progress: &dyn Fn(f32),
    cancel: &AtomicBool,
) -> Result<PathBuf, String> {
    options.validate()?;
    if cancel.load(Ordering::Relaxed) {
        return Err("Export cancelled".into());
    }
    let settings = &options.render;
    let rate = settings.sample_rate as f32;
    let config = EngineConfig {
        max_block: 512,
        ..EngineConfig::default()
    };
    let mut engine = OfflineEngine::new(rate, &config);

    let mut slots: HashMap<Id, Slot> = HashMap::new();
    let devices = project
        .channels
        .iter()
        .map(|c| &c.device)
        .chain(project.mixer.tracks.iter().flat_map(|t| t.inserts.iter()));
    for (i, inst) in devices.enumerate() {
        let Some(mut device) = prebuilt
            .remove(&inst.id)
            .or_else(|| peel_devices::create(&inst.type_id))
        else {
            continue;
        };
        let desc = device.descriptor();
        for (k, v) in &inst.params {
            if let Some(p) = desc.param_by_key(k) {
                device.set_param(p.id, *v);
            }
        }
        device.prepare(rate, config.max_block);
        if let Some(asset) = inst.sample
            && let Some(buf) = samples.get(&asset)
        {
            let _ = device.set_sample(Some(buf.clone()));
        }
        let slot = i as Slot;
        engine.send(EngineCommand::InstallDevice { slot, device });
        if let Some(error) = engine.compensation_error() {
            return Err(error.to_string());
        }
        slots.insert(inst.id, slot);
    }
    let meters: HashMap<Id, u16> = project
        .mixer
        .tracks
        .iter()
        .enumerate()
        .map(|(i, t)| (t.id, i as u16))
        .collect();
    let audio = clip_audio(project, samples, rate)?;
    let (mut plan, _) = compile(
        project,
        &CompileContext {
            mode: PlayMode::Song,
            slots: &slots,
            meters: &meters,
            samples: &audio,
        },
    );
    plan.loop_range = None;
    let tempo = plan.tempo.clone();
    let end_beat = project.arrangement().end().as_f64();
    if end_beat <= 0.0 {
        return Err("The arrangement is empty. Place patterns in the playlist first.".into());
    }
    let frames = ((tempo.beat_to_seconds(end_beat) + f64::from(settings.tail_seconds))
        * f64::from(settings.sample_rate))
    .ceil();
    if !frames.is_finite()
        || frames <= 0.0
        || frames > (isize::MAX as usize / size_of::<f32>()) as f64
    {
        return Err("The arrangement is too long to export".into());
    }
    let total = frames as usize;
    engine.try_set_plan(Box::new(plan))?;
    engine.send(EngineCommand::Seek { beat: 0.0 });
    engine.send(EngineCommand::Play);

    let allocate = || -> Result<Vec<f32>, String> {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(total)
            .map_err(|_| "Not enough memory to export this arrangement".to_string())?;
        buffer.resize(total, 0.0);
        Ok(buffer)
    };
    let mut left = allocate()?;
    let mut right = allocate()?;
    // Render through the complete compensated graph, then discard its leading
    // latency. Export duration and the arrangement's sample zero stay exact.
    let mut warmup_left = vec![0.0; engine.max_block()];
    let mut warmup_right = vec![0.0; engine.max_block()];
    let mut remaining = engine.compensation_latency_samples() as usize;
    while remaining > 0 {
        if cancel.load(Ordering::Relaxed) {
            return Err("Export cancelled".into());
        }
        let n = remaining.min(warmup_left.len());
        engine.render(&mut warmup_left[..n], &mut warmup_right[..n]);
        remaining -= n;
        engine.drain_retired();
    }
    let chunk = 4096;
    let mut pos = 0;
    while pos < total {
        if cancel.load(Ordering::Relaxed) {
            return Err("Export cancelled".into());
        }
        let n = chunk.min(total - pos);
        engine.render(&mut left[pos..pos + n], &mut right[pos..pos + n]);
        pos += n;
        progress(pos as f32 / total as f32 * 0.95);
        engine.drain_retired();
    }
    process_output(&mut left, &mut right, options, cancel)?;
    let channels: Vec<&[f32]> = if options.mono {
        vec![&left]
    } else {
        vec![&left, &right]
    };
    peel_media::write_audio(
        &settings.path,
        &channels,
        settings.sample_rate,
        options.encoding(),
        cancel,
    )
    .map_err(|e| e.to_string())?;
    progress(1.0);
    Ok(settings.path.clone())
}

fn process_output(
    left: &mut [f32],
    right: &mut [f32],
    options: &AudioExportSettings,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let mut peak = 0.0f32;
    for (ls, rs) in left.chunks_mut(4096).zip(right.chunks_mut(4096)) {
        if cancel.load(Ordering::Relaxed) {
            return Err("Export cancelled".into());
        }
        for (l, r) in ls.iter_mut().zip(rs) {
            if !l.is_finite() {
                *l = 0.0;
            }
            if !r.is_finite() {
                *r = 0.0;
            }
            if options.mono {
                *l = *l * 0.5 + *r * 0.5;
            }
            peak = peak.max(l.abs());
            if !options.mono {
                peak = peak.max(r.abs());
            }
        }
    }
    if options.normalize && peak > 0.0 {
        let gain = f64::from(peel_dsp::util::db_to_gain(-1.0)) / f64::from(peak);
        for (ls, rs) in left.chunks_mut(4096).zip(right.chunks_mut(4096)) {
            if cancel.load(Ordering::Relaxed) {
                return Err("Export cancelled".into());
            }
            for (l, r) in ls.iter_mut().zip(rs) {
                *l = (f64::from(*l) * gain) as f32;
                *r = (f64::from(*r) * gain) as f32;
            }
        }
    }
    Ok(())
}

/// Decoded audio for the arrangement's audible audio clips at the export
/// rate. A clip whose audio is missing is an error rather than a silent gap
/// in the finished file.
fn clip_audio(
    project: &Project,
    samples: &HashMap<Id, Arc<SampleBuffer>>,
    rate: f32,
) -> Result<HashMap<Id, Arc<SampleBuffer>>, String> {
    let arr = project.arrangement();
    let mut out = HashMap::new();
    for clip in &arr.clips {
        let ClipContent::Audio { asset, .. } = &clip.content else {
            continue;
        };
        let lane_muted = arr.tracks.iter().any(|t| t.id == clip.track && t.muted);
        if clip.muted || lane_muted || out.contains_key(asset) {
            continue;
        }
        let Some(buf) = samples.get(asset) else {
            let name = project.asset(*asset).map_or("An audio file", |a| &a.name);
            return Err(format!(
                "{name} could not be loaded, so it cannot be exported."
            ));
        };
        let buf = if (buf.sample_rate - rate).abs() < 0.5 {
            buf.clone()
        } else {
            Arc::new(peel_media::resample(buf, rate))
        };
        out.insert(*asset, buf);
    }
    // Tempo-following clips play stretched copies.
    let tempo = peel_time::TempoMap::from_segments(&project.tempo);
    for clip in &arr.clips {
        let (ClipContent::Audio { asset, .. }, Some(ratio)) =
            (&clip.content, crate::compile::stretch_ratio(&tempo, clip))
        else {
            continue;
        };
        let key = crate::compile::stretched_key(*asset, ratio);
        if out.contains_key(&key) {
            continue;
        }
        if let Some(src) = out.get(asset).cloned() {
            out.insert(key, Arc::new(stretched(&src, ratio)));
        }
    }
    Ok(out)
}

/// A time-stretched copy of a sample (pitch kept).
pub(crate) fn stretched(src: &SampleBuffer, ratio: f64) -> SampleBuffer {
    let channels = peel_dsp::stretch::stretch(&src.channels, ratio, src.sample_rate);
    let mut out = SampleBuffer::new(src.name.clone(), src.sample_rate, channels);
    out.root_key = src.root_key;
    out
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn mono_averages_channels_before_normalizing_and_silence_stays_finite() {
        let options = AudioExportSettings {
            mono: true,
            normalize: true,
            ..Default::default()
        };
        let (mut left, mut right) = ([0.5, -0.5, f32::NAN], [0.25, 0.5, f32::INFINITY]);
        process_output(&mut left, &mut right, &options, &AtomicBool::new(false)).unwrap();
        assert!((left[0] - peel_dsp::util::db_to_gain(-1.0)).abs() < 1e-6);
        assert_eq!(left[1], 0.0, "opposite channels cancel in mono");
        assert_eq!(left[2], 0.0);
        let (mut left, mut right) = ([0.0; 3], [0.0; 3]);
        process_output(&mut left, &mut right, &options, &AtomicBool::new(false)).unwrap();
        assert_eq!(left, [0.0; 3]);
    }

    #[test]
    fn stereo_normalization_preserves_channel_balance_and_float_headroom() {
        let mut options = AudioExportSettings {
            normalize: true,
            ..Default::default()
        };
        let (mut left, mut right) = ([2.0, -1.0], [1.0, -0.5]);
        process_output(&mut left, &mut right, &options, &AtomicBool::new(false)).unwrap();
        assert!((left[0] - peel_dsp::util::db_to_gain(-1.0)).abs() < 1e-6);
        assert_eq!(left[0], right[0] * 2.0);
        assert_eq!(left[1], right[1] * 2.0);
        options.normalize = false;
        let (mut left, mut right) = ([2.0], [-3.0]);
        process_output(&mut left, &mut right, &options, &AtomicBool::new(false)).unwrap();
        assert_eq!(left, [2.0]);
        assert_eq!(right, [-3.0]);
    }
    #[test]
    fn compensated_exports_keep_sample_zero_final_sample_and_exact_length() {
        use peel_document::{Clip, DeviceInstance, Note, Time};
        use peel_dsp::{
            DeviceDescriptor, DeviceEvent, DeviceKind, EventKind, ParamId, ProcessContext,
        };
        struct Impulse {
            ring: [f32; 37],
            cursor: usize,
        }
        static DESCRIPTOR: DeviceDescriptor = DeviceDescriptor {
            type_id: "test.latent-impulse",
            name: "Impulse",
            description: "",
            category: "Test",
            kind: DeviceKind::Instrument,
            version: 1,
            params: &[],
        };
        impl Device for Impulse {
            fn descriptor(&self) -> &'static DeviceDescriptor {
                &DESCRIPTOR
            }
            fn prepare(&mut self, _: f32, _: usize) {}
            fn reset(&mut self) {
                self.ring.fill(0.0);
                self.cursor = 0;
            }
            fn set_param(&mut self, _: ParamId, _: f32) {}
            fn param(&self, _: ParamId) -> f32 {
                0.0
            }
            fn latency(&self) -> u32 {
                37
            }
            fn process(
                &mut self,
                _: &ProcessContext,
                events: &[DeviceEvent],
                l: &mut [f32],
                r: &mut [f32],
            ) {
                for i in 0..l.len() {
                    let input = if events.iter().any(|e| {
                        e.offset as usize == i && matches!(e.kind, EventKind::NoteOn { .. })
                    }) {
                        0.25
                    } else {
                        0.0
                    };
                    l[i] = self.ring[self.cursor];
                    r[i] = l[i];
                    self.ring[self.cursor] = input;
                    self.cursor = (self.cursor + 1) % self.ring.len();
                }
            }
        }
        let mut project = Project::empty("PDC export");
        project.set_bpm(120.0);
        let inst = DeviceInstance::new(DESCRIPTOR.type_id, 1);
        let instance = inst.id;
        let channel = crate::template::add_channel(&mut project, "Impulse", 0, inst, 60);
        let pattern = Arc::make_mut(&mut project.patterns[0]);
        pattern.length = Time::beats(1);
        pattern.notes = vec![
            Note::new(channel, Time::ZERO, Time::new(1, 24_000), 60, 1.0),
            Note::new(
                channel,
                Time::new(23_999, 24_000),
                Time::new(1, 24_000),
                60,
                1.0,
            ),
        ];
        let pattern = pattern.id;
        let arrangement = project.arrangement_mut();
        arrangement.clips.push(Clip {
            id: Id::new(),
            track: arrangement.tracks[0].id,
            start: Time::ZERO,
            length: Time::beats(1),
            offset: Time::ZERO,
            muted: false,
            content: ClipContent::Pattern { pattern },
        });
        let path = std::env::temp_dir().join(format!("peel-pdc-{}.wav", Id::new()));
        let settings = AudioExportSettings {
            render: ExportSettings {
                path: path.clone(),
                sample_rate: 48_000,
                bits: 32,
                tail_seconds: 0.0,
            },
            dither: false,
            ..Default::default()
        };
        let mut prebuilt = Prebuilt::new();
        prebuilt.insert(
            instance,
            Box::new(Impulse {
                ring: [0.0; 37],
                cursor: 0,
            }),
        );
        render_with(
            &project,
            &HashMap::new(),
            prebuilt,
            &settings,
            &|_| {},
            &AtomicBool::new(false),
        )
        .unwrap();
        let decoded = peel_media::decode_file(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(decoded.channels[0].len(), 24_000);
        assert_eq!(decoded.channels[0][0], 0.25);
        assert_eq!(decoded.channels[0][23_999], 0.25);
        assert!(decoded.channels[0][1..23_999].iter().all(|&s| s == 0.0));
    }
}
