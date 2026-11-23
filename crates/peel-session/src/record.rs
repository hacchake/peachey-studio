//! Audio input and recording onto Playlist lanes.
//!
//! # The flow
//!
//! 1. Choose an input ([`Session::set_input_device`]) and its channels
//!    ([`Session::set_input_channels`]). Opening the input is what makes the
//!    operating system ask for microphone permission. The input meter
//!    ([`Session::input_level`]) and software monitoring
//!    ([`Session::set_monitoring`]) work as soon as it is open.
//! 2. Arm one or more lanes ([`Session::arm_lane`]).
//! 3. [`Session::start_recording`] (in Song mode) starts the transport if it
//!    is not running and begins a take. The take is written to a WAV file
//!    *while it is captured* by a writer thread of its own, so a stalled UI
//!    cannot lose audio and a crash loses at most a fraction of a second; the
//!    file is a valid WAV from the first moment (see
//!    [`peel_media::WavRecorder`]).
//! 4. [`Session::stop_recording`] (or [`Session::stop`], which ends the take
//!    too) finishes the file and adds one audio clip per armed lane, all
//!    referencing the new asset, as a single undoable edit named "Record
//!    audio". Undo removes the clips; the recorded file stays on disk.
//!
//! # Where files go
//!
//! A saved project records into `Song.peeldata/recordings/` and, when the
//! take ends, copies the file into its managed media (`media/<hash>.wav`) so
//! the project is self-contained; the original stays in `recordings/` as the
//! recovery copy. An unsaved project records into `Peel Studio/Recordings`
//! under the user's data folder and references the file in place.
//! [`Session::set_recordings_dir`] overrides both.
//!
//! # Where the clip goes
//!
//! The first captured frame is placed where the player heard the music when
//! they played it: the playhead at the first input callback, moved back by the
//! output and input latency and the user's alignment offset. That formula is
//! [`peel_engine::take_start_beat`] and is documented in
//! [`peel_engine::latency`]. Takes that would start before the top of the song
//! start at zero with the head trimmed (the clip's `offset`).
//!
//! # Limits
//!
//! * Audio recording is a Song-mode feature.
//! * A take is one linear recording. While it runs the transport ignores the
//!   arrangement's loop range and the end of the song (the plan is compiled
//!   without them and restored when the take ends), so recording past the
//!   last clip extends the song instead of wrapping. Loop takes with take
//!   lanes are not implemented.
//! * The input and output run on their own clocks; with two different devices
//!   a long take can drift against the playback by a few milliseconds a
//!   minute. Use one interface for both when that matters.
//! * Takes use 32-bit float WAV, which holds about three hours of stereo at
//!   48 kHz.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use peel_document::{AudioAsset, Id, Time};
use peel_engine::{
    InputChannels, InputDeviceInfo, InputInfo, InputInjector, InputRequest, InputShared,
    LatencyReport, list_input_devices, take_start_beat,
};
use peel_media::{RecordedWav, WavRecorder};
use peel_project::AudioProbe;
use peel_time::TempoMap;

use crate::audio::Placement;
use crate::{PlayMode, Session};

/// Denominator of recorded clip positions: a clip lands within a fifteenth of
/// a millisecond of where it was played, at any realistic tempo.
const TIME_GRID: i64 = 16_384;
/// How often the writer asks the operating system for the data it has.
const CHECKPOINT_EVERY: Duration = Duration::from_millis(100);
/// How often the writer forces the file to disk.
const SYNC_EVERY: Duration = Duration::from_secs(1);
/// How long the writer sleeps when the ring is empty.
const WRITER_IDLE: Duration = Duration::from_millis(5);
/// Samples the writer moves per pass.
const WRITER_BUFFER: usize = 1 << 15;

/// What the recording strip shows while a take runs.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingStatus {
    /// The file being written.
    pub path: PathBuf,
    /// Seconds of audio on disk so far.
    pub seconds: f64,
    pub frames: u64,
    /// The take is armed but the transport has not started yet.
    pub waiting_for_transport: bool,
    /// Frames lost because the disk could not keep up for two seconds (the
    /// take is padded with silence of the same length).
    pub dropped_frames: u64,
    /// Why the take cannot continue, if it cannot (disk full, device lost).
    pub error: Option<String>,
    /// The lanes that will get a clip.
    pub lanes: Vec<Id>,
}

/// Software input monitoring: the state, whether it can work, and its delay.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorStatus {
    pub on: bool,
    /// Linear gain.
    pub gain: f32,
    /// Monitoring can run now (an input is open with a connected monitor FIFO).
    pub available: bool,
    /// What to do when it cannot.
    pub unavailable_reason: Option<String>,
    /// Estimated delay from the instrument to the headphones, in
    /// milliseconds: input device, monitor FIFO and output device.
    pub latency_ms: f32,
    /// Times the output ran out of input and played silence.
    pub underruns: u64,
    /// Frames discarded because one side stalled.
    pub dropped_frames: u64,
}

/// Session state for the audio input and recording.
pub(crate) struct RecordState {
    armed: Vec<Id>,
    channels: InputChannels,
    monitor_on: bool,
    monitor_gain: f32,
    offset_ms: f32,
    recordings_dir: Option<PathBuf>,
    /// The user chose an input; reopen it after the audio engine restarts.
    input_wanted: bool,
    active: Option<Active>,
    /// Clips of the take that ended, until [`Session::stop_recording`] hands
    /// them out.
    finished: Vec<Id>,
}

impl Default for RecordState {
    fn default() -> Self {
        RecordState {
            armed: Vec::new(),
            channels: InputChannels::default(),
            monitor_on: false,
            monitor_gain: 1.0,
            offset_ms: 0.0,
            recordings_dir: None,
            input_wanted: false,
            active: None,
            finished: Vec::new(),
        }
    }
}

// ----------------------------------------------------------------------
// The writer thread
// ----------------------------------------------------------------------

#[derive(Default)]
struct WriterControl {
    stop: AtomicBool,
    frames: AtomicU64,
    error: Mutex<Option<String>>,
}

impl WriterControl {
    fn set_error(&self, message: String) {
        let mut slot = self.error.lock().unwrap_or_else(PoisonError::into_inner);
        slot.get_or_insert(message);
    }

    fn error(&self) -> Option<String> {
        self.error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

struct WriterOutcome {
    wav: Option<RecordedWav>,
    error: Option<String>,
}

/// Drains the capture ring into the WAV until told to stop and the ring is
/// empty. After a write error it keeps draining (so the input never blocks
/// on a full ring) and discards.
fn run_writer(
    shared: Arc<InputShared>,
    mut wav: WavRecorder,
    ctl: Arc<WriterControl>,
    mono: bool,
) -> WriterOutcome {
    let mut stereo = vec![0.0f32; WRITER_BUFFER];
    let mut single: Vec<f32> = Vec::with_capacity(if mono { WRITER_BUFFER / 2 } else { 0 });
    let mut failed = false;
    let mut last_checkpoint = Instant::now();
    let mut last_sync = Instant::now();
    loop {
        let n = shared.drain_capture(&mut stereo);
        if n > 0 && !failed {
            let result = if mono {
                single.clear();
                single.extend(stereo[..n].as_chunks::<2>().0.iter().map(|pair| pair[0]));
                wav.write(&single)
            } else {
                wav.write(&stereo[..n])
            };
            match result {
                Ok(()) => ctl.frames.store(wav.frames(), Ordering::Release),
                Err(e) => {
                    ctl.set_error(format!("could not write the recording: {e}"));
                    failed = true;
                    shared.end_capture();
                }
            }
        }
        if !failed {
            let now = Instant::now();
            if now.duration_since(last_sync) >= SYNC_EVERY {
                if let Err(e) = wav.sync() {
                    ctl.set_error(format!("could not save the recording: {e}"));
                    failed = true;
                }
                last_sync = now;
                last_checkpoint = now;
            } else if now.duration_since(last_checkpoint) >= CHECKPOINT_EVERY {
                if let Err(e) = wav.checkpoint() {
                    ctl.set_error(format!("could not save the recording: {e}"));
                    failed = true;
                }
                last_checkpoint = now;
            }
        }
        if n == 0 {
            if ctl.stop.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(WRITER_IDLE);
        }
    }
    match wav.finalize() {
        Ok(done) => WriterOutcome {
            wav: Some(done),
            error: ctl.error(),
        },
        Err(e) => WriterOutcome {
            wav: None,
            error: Some(ctl.error().unwrap_or_else(|| e.to_string())),
        },
    }
}

/// A take in progress.
struct Active {
    shared: Arc<InputShared>,
    ctl: Arc<WriterControl>,
    writer: Option<JoinHandle<WriterOutcome>>,
    path: PathBuf,
    lanes: Vec<Id>,
}

impl Active {
    /// Ends capture, lets the writer drain what is left and finish the file.
    /// `None` if it was already finished or the writer thread died.
    fn finish(&mut self) -> Option<WriterOutcome> {
        let writer = self.writer.take()?;
        // A callback that is running now may still push frames. Wait for the
        // next one to start before declaring the ring final; a stalled device
        // gets a short grace period instead.
        let before = self.shared.callbacks();
        self.shared.end_capture();
        let patience =
            Duration::from_secs_f64((self.shared.input_block_seconds() * 8.0).clamp(0.03, 0.25));
        let t0 = Instant::now();
        while self.shared.callbacks() == before
            && self.shared.failure().is_none()
            && t0.elapsed() < patience
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.ctl.stop.store(true, Ordering::Release);
        writer.join().ok()
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        // A session dropped mid-take still closes the file properly.
        let _ = self.finish();
    }
}

// ----------------------------------------------------------------------
// Where a take goes on the timeline
// ----------------------------------------------------------------------

/// Start, length and offset of the clip for a take of `seconds` of audio
/// whose first frame belongs at `start_beat` (which may be negative).
///
/// A take that starts before the top of the song starts its clip at zero and
/// skips the head through the clip's `offset`. `None` when nothing of the
/// take would be audible.
fn placement_for_take(
    tempo: &TempoMap,
    start_beat: f64,
    seconds: f64,
) -> Option<(Time, Time, Time)> {
    if seconds.is_nan() || seconds <= 0.0 || !start_beat.is_finite() {
        return None;
    }
    let grid = TIME_GRID as f64;
    let (start, offset, audible) = if start_beat >= 0.0 {
        (Time::from_f64(start_beat, TIME_GRID), Time::ZERO, seconds)
    } else {
        let trimmed = (-tempo.beat_to_seconds(start_beat)).max(0.0);
        let offset = Time::from_f64(trimmed * tempo.bpm_at(0.0) / 60.0, TIME_GRID);
        (Time::ZERO, offset, seconds - trimmed)
    };
    if audible <= 0.0 {
        return None;
    }
    let from = start.as_f64();
    let end = tempo.seconds_to_beat(tempo.beat_to_seconds(from) + audible);
    // The epsilon keeps a length that is exactly on the grid from rounding up
    // a tick because of floating point error.
    let ticks = (((end - from) * grid - 1e-6).ceil() as i64).max(1);
    Some((start, Time::new(ticks, TIME_GRID), offset))
}

/// BLAKE3 of a file, read in pieces.
fn hash_file(path: &Path) -> Result<String, String> {
    let file = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    hasher
        .update_reader(file)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(hasher.finalize().to_hex().to_string())
}

// ----------------------------------------------------------------------
// Session API
// ----------------------------------------------------------------------

impl Session {
    // ---- the input device ----

    /// The input devices the system offers. Enumeration queries every device
    /// (tens of milliseconds) but opens none, so it never triggers a
    /// microphone permission prompt.
    pub fn input_devices(&self) -> Vec<InputDeviceInfo> {
        list_input_devices()
    }

    /// The input last asked for (`None` is the system default).
    pub fn requested_input_device(&self) -> Option<&str> {
        self.io.config.input_device.as_deref()
    }

    /// Opens the input `device` names (`None` is the system default),
    /// replacing the one that was open. This is where the operating system
    /// asks for microphone permission. The error says what to do next
    /// (reconnect the device, allow microphone access) and is also posted as
    /// a notice.
    ///
    /// Refused while recording.
    pub fn set_input_device(&mut self, device: Option<String>) -> Result<(), String> {
        if self.is_recording() {
            return Err(self.refuse("Stop recording before changing the input."));
        }
        self.io.config.input_device = device.filter(|d| !d.trim().is_empty());
        self.open_input_now().map(|_| ()).inspect_err(|e| {
            self.notify(e.clone(), true);
        })
    }

    /// Remembers which input to open when one is next needed (arming a
    /// lane), without opening it now, so no permission prompt appears at
    /// launch.
    pub fn prefer_input_device(&mut self, device: Option<String>) {
        self.io.config.input_device = device.filter(|d| !d.trim().is_empty());
    }

    /// Closes the input (meter, monitoring and arming stop working until one
    /// is opened again). Refused while recording.
    pub fn close_input(&mut self) {
        if self.is_recording() {
            self.refuse("Stop recording before closing the input.");
            return;
        }
        self.engine.close_input();
        self.rec.input_wanted = false;
    }

    /// An input is open.
    pub fn input_is_open(&self) -> bool {
        self.engine.input().is_open()
    }

    /// The open input: device, sample rate and how many channels it offers.
    pub fn input_info(&self) -> Option<InputInfo> {
        self.engine.input_info().cloned()
    }

    /// Chooses the device channels a take records and the meter and monitor
    /// follow: one channel (a mono file) or a pair (a stereo file). Indices
    /// past the device's channels are clamped. A change of channel *count* is
    /// ignored while recording, because the file is already open.
    pub fn set_input_channels(&mut self, channels: InputChannels) {
        if self.is_recording()
            && let Some(active) = &self.rec.active
            && active.shared.channels().count() != channels.count()
        {
            self.refuse("Stop recording before switching between mono and stereo.");
            return;
        }
        self.rec.channels = channels;
        self.engine.input().set_channels(channels);
    }

    /// The channel selection, clamped to the open device.
    pub fn input_channels(&self) -> InputChannels {
        if self.input_is_open() {
            self.engine.input().channels()
        } else {
            self.rec.channels
        }
    }

    /// Why input is not working, once the device is lost or its stream
    /// fails. Open the input again after fixing the cause.
    pub fn input_error(&self) -> Option<String> {
        let shared = self.engine.input();
        shared.failure().map(|_| {
            shared
                .last_error()
                .unwrap_or_else(|| "The audio input stopped working".to_string())
        })
    }

    /// A reason to look at the input, if there is one: it failed, or it has
    /// delivered nothing but digital silence for a few seconds (muted
    /// hardware, an unplugged interface, or macOS withholding microphone
    /// access, which it does by sending silence). For a status line next to
    /// the meter; `None` when all is well or no input is open.
    pub fn input_warning(&self) -> Option<String> {
        if let Some(why) = self.input_error() {
            return Some(why);
        }
        let shared = self.engine.input();
        let listened = shared.callbacks() as f64 * shared.input_block_seconds();
        (shared.is_open() && listened >= 3.0 && !shared.heard_signal()).then(|| {
            "The input has been completely silent. Check the cable, the interface's gain and mute, and that Peachey Studio may use the microphone (System Settings, Privacy & Security, Microphone)."
                .to_string()
        })
    }

    /// Peak level of the input since the last call, left and right (the same
    /// for a mono selection), 0..=1 and beyond for clipping. Like the other
    /// meters, reading clears it; call once per frame.
    pub fn input_level(&self) -> (f32, f32) {
        self.engine.input().take_peak()
    }

    /// Input and output latency as last measured, for display.
    pub fn input_latency(&self) -> LatencyReport {
        self.engine.input().latency()
    }

    /// An input that no device feeds: push frames into the returned
    /// [`InputInjector`] and they flow through the same capture, meter and
    /// monitor path a microphone uses. For tests and headless tools.
    pub fn open_injected_input(&mut self, sample_rate: u32, channels: u16) -> InputInjector {
        let injector = self.engine.open_injected_input(sample_rate, channels);
        self.apply_input_settings();
        injector
    }

    fn open_input_now(&mut self) -> Result<InputInfo, String> {
        if self.io.silent {
            return Err("Audio input is not available without an audio device.".into());
        }
        let request = InputRequest::from_config(&self.io.config);
        let info = self
            .engine
            .open_input(&request)
            .map_err(|e| e.to_string())?;
        self.rec.input_wanted = true;
        self.apply_input_settings();
        Ok(info)
    }

    fn apply_input_settings(&self) {
        let shared = self.engine.input();
        shared.set_channels(self.rec.channels);
        shared.set_monitoring(self.rec.monitor_on, self.rec.monitor_gain);
    }

    /// Brings the input back on a new audio engine.
    pub(crate) fn reopen_input(&mut self) {
        if self.rec.input_wanted
            && !self.io.silent
            && let Err(e) = self.open_input_now()
        {
            self.rec.input_wanted = false;
            self.notify(format!("Audio input closed: {e}"), true);
        }
    }

    // ---- monitoring ----

    /// Turns software input monitoring on or off. `gain` is linear (1.0 is
    /// unity). Monitoring is off by default and independent of arming and of
    /// the meter. It adds the input to the output after the master, so the
    /// player hears themselves one input buffer, one output buffer and a
    /// small jitter FIFO late; [`monitor_status`](Session::monitor_status)
    /// reports the total. With an interface that can monitor in hardware,
    /// leave this off to avoid hearing a doubled signal.
    pub fn set_monitoring(&mut self, on: bool, gain: f32) {
        self.rec.monitor_on = on;
        if gain.is_finite() {
            self.rec.monitor_gain = gain.clamp(0.0, 4.0);
        }
        self.engine
            .input()
            .set_monitoring(self.rec.monitor_on, self.rec.monitor_gain);
        if on && let Some(why) = self.monitor_status().unavailable_reason {
            self.notify(why, true);
        }
    }

    /// The monitoring switch and gain as set.
    pub fn monitoring(&self) -> (bool, f32) {
        (self.rec.monitor_on, self.rec.monitor_gain)
    }

    pub fn monitor_status(&self) -> MonitorStatus {
        let shared = self.engine.input();
        let out_rate = self.engine.sample_rate();
        let reason = if !shared.is_open() {
            Some("No audio input is open. Choose an input to monitor it.".to_string())
        } else if !shared.monitor_supported() {
            Some("Input monitoring is not connected. Reopen the input device or monitor in hardware.".to_string())
        } else {
            None
        };
        let stats = shared.monitor_stats();
        let lat = shared.latency();
        let fifo_s = f64::from(stats.fifo_frames) / f64::from(out_rate.max(1.0));
        MonitorStatus {
            on: self.rec.monitor_on,
            gain: self.rec.monitor_gain,
            available: reason.is_none(),
            unavailable_reason: reason,
            latency_ms: ((lat.input_s + fifo_s + lat.output_s) * 1000.0) as f32,
            underruns: stats.underruns,
            dropped_frames: stats.dropped_frames,
        }
    }

    // ---- settings ----

    /// Alignment correction in milliseconds: positive moves new takes earlier
    /// (use it when recordings arrive late). It adds to the latency the
    /// devices report; measure it with a loopback cable or set it by ear.
    pub fn set_record_offset_ms(&mut self, ms: f32) {
        self.rec.offset_ms = if ms.is_finite() {
            ms.clamp(-1000.0, 1000.0)
        } else {
            0.0
        };
    }

    pub fn record_offset_ms(&self) -> f32 {
        self.rec.offset_ms
    }

    /// Where takes are written, replacing the defaults described in the
    /// module documentation. `None` restores them.
    pub fn set_recordings_dir(&mut self, dir: Option<PathBuf>) {
        self.rec.recordings_dir = dir;
    }

    /// The folder the next take is written to.
    pub fn recordings_dir(&self) -> PathBuf {
        if let Some(dir) = &self.rec.recordings_dir {
            return dir.clone();
        }
        match &self.path {
            Some(project) => peel_project::data_dir_for(project).join("recordings"),
            None => dirs::data_dir()
                .unwrap_or_else(std::env::temp_dir)
                .join("Peel Studio")
                .join("Recordings"),
        }
    }

    // ---- arming ----

    /// Arms or disarms a Playlist lane for audio recording. Arming opens the
    /// input if none is open (and posts a notice if that fails). Armed lanes
    /// are not part of the project and are not undoable; the lanes armed when
    /// a take starts are the ones that get its clips.
    pub fn arm_lane(&mut self, lane: Id, armed: bool) {
        if !armed {
            self.rec.armed.retain(|l| *l != lane);
            return;
        }
        if !self.lane_exists(lane) {
            return;
        }
        if !self.rec.armed.contains(&lane) {
            self.rec.armed.push(lane);
        }
        if !self.input_is_open()
            && !self.io.silent
            && !self.is_recording()
            && let Err(e) = self.open_input_now()
        {
            self.notify(e, true);
        }
    }

    /// The armed lanes that still exist, in the order they were armed.
    pub fn armed_lanes(&self) -> Vec<Id> {
        self.rec
            .armed
            .iter()
            .copied()
            .filter(|l| self.lane_exists(*l))
            .collect()
    }

    /// Drops the armed lanes and the clips of a finished take: they belong to
    /// the project that is being replaced.
    pub(crate) fn forget_lanes(&mut self) {
        self.rec.armed.clear();
        self.rec.finished.clear();
    }

    pub fn is_lane_armed(&self, lane: Id) -> bool {
        self.rec.armed.contains(&lane) && self.lane_exists(lane)
    }

    fn lane_exists(&self, lane: Id) -> bool {
        self.doc
            .project()
            .arrangement()
            .tracks
            .iter()
            .any(|t| t.id == lane)
    }

    // ---- recording ----

    /// Why a take cannot start now, as a message that says what to do, or
    /// `None` when it can. Use it for the Record button's tooltip.
    pub fn record_blocker(&self) -> Option<String> {
        if self.is_recording() {
            return Some("Already recording.".into());
        }
        if self.mode != PlayMode::Song {
            return Some("Switch to Song mode to record audio onto a Playlist lane.".into());
        }
        if self.armed_lanes().is_empty() {
            return Some("Arm a Playlist lane to choose where the recording goes.".into());
        }
        if !self.input_is_open() {
            return Some("No audio input is open. Choose an input device first.".into());
        }
        if let Some(why) = self.input_error() {
            return Some(format!(
                "{why}. Reconnect the device or choose the input again."
            ));
        }
        None
    }

    /// Starts a take on the armed lanes. If the transport is stopped it
    /// starts too, so call this when Record and Play are pressed together; if
    /// it already runs the take begins at the next input callback.
    ///
    /// Nothing is recorded before the transport runs. The error says what to
    /// fix (see [`record_blocker`](Session::record_blocker)) and is also
    /// posted as a notice.
    pub fn start_recording(&mut self) -> Result<(), String> {
        if let Some(why) = self.record_blocker() {
            return Err(self.refuse(why));
        }
        let shared = self.engine.input().clone();
        let channels = shared.channels().count();
        let rate = shared.input_sample_rate();
        let id = Id::new().as_u128();
        let path = self
            .recordings_dir()
            .join(format!("recording-{:08x}.wav", id as u32));
        let wav = WavRecorder::create(&path, channels, rate).map_err(|e| {
            self.refuse(format!(
                "Could not start recording: {e}. Check that the folder exists and the disk has free space."
            ))
        })?;

        let ctl = Arc::new(WriterControl::default());
        let writer = {
            let (shared, ctl) = (shared.clone(), ctl.clone());
            std::thread::Builder::new()
                .name("peel-record-writer".into())
                .spawn(move || run_writer(shared, wav, ctl, channels == 1))
        };
        let writer = match writer {
            Ok(w) => w,
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                return Err(self.refuse(format!("Could not start the recording thread: {e}")));
            }
        };
        let lanes = self.armed_lanes();
        shared.arm_capture();
        self.rec.finished.clear();
        self.rec.active = Some(Active {
            shared,
            ctl,
            writer: Some(writer),
            path,
            lanes,
        });
        // Compile the plan without the loop and the end before playing.
        self.synced = None;
        if self.playing {
            self.sync();
        } else {
            self.play();
        }
        Ok(())
    }

    /// Ends the take and returns the clips it created, one per armed lane
    /// (empty when nothing was recorded; a notice says why). Also returns the
    /// clips of a take that already ended because the transport stopped or
    /// the input failed, once.
    pub fn stop_recording(&mut self) -> Vec<Id> {
        self.finish_recording(true);
        std::mem::take(&mut self.rec.finished)
    }

    /// Ends the take without adding clips. The audio stays in the recordings
    /// folder.
    pub fn cancel_recording(&mut self) {
        self.finish_recording(false);
        self.rec.finished.clear();
    }

    /// A take is running (or waiting for the transport).
    pub fn is_recording(&self) -> bool {
        self.rec.active.is_some()
    }

    /// Progress of the running take.
    pub fn recording_status(&self) -> Option<RecordingStatus> {
        let active = self.rec.active.as_ref()?;
        let frames = active.ctl.frames.load(Ordering::Acquire);
        let rate = f64::from(active.shared.input_sample_rate().max(1));
        let state = active.shared.capture_state();
        Some(RecordingStatus {
            path: active.path.clone(),
            seconds: frames as f64 / rate,
            frames,
            waiting_for_transport: state == peel_engine::CaptureState::Armed,
            dropped_frames: active.shared.dropped_frames(),
            error: active.ctl.error().or_else(|| self.input_error()),
            lanes: active.lanes.clone(),
        })
    }

    /// Per-frame upkeep: a take whose device failed or whose disk filled up
    /// ends here, keeping what was captured.
    pub(crate) fn poll_recording(&mut self) {
        let Some(active) = self.rec.active.as_ref() else {
            return;
        };
        let problem = active.ctl.error().or_else(|| {
            active.shared.failure().map(|_| {
                active
                    .shared
                    .last_error()
                    .unwrap_or_else(|| "the input device stopped".to_string())
            })
        });
        if let Some(why) = problem {
            self.finish_recording_with(true, Some(why));
        }
    }

    pub(crate) fn finish_recording(&mut self, place: bool) {
        self.finish_recording_with(place, None);
    }

    /// The plan must run straight through (no loop, no end): a take is in
    /// progress in Song mode.
    pub(crate) fn recording_free_runs(&self) -> bool {
        self.rec.active.is_some() && self.mode == PlayMode::Song
    }

    /// Ends the take, if one runs. With `place`, adds the clips and queues
    /// their ids for [`stop_recording`](Session::stop_recording). `problem`
    /// is why the take ended early, for the notice.
    fn finish_recording_with(&mut self, place: bool, problem: Option<String>) {
        if self.rec.active.is_none() {
            return;
        }
        self.end_take(place, problem);
        // Back to the song's own loop and end (a placed clip recompiles too).
        self.synced = None;
        self.sync();
    }

    fn end_take(&mut self, place: bool, problem: Option<String>) {
        let Some(mut active) = self.rec.active.take() else {
            return;
        };
        let outcome = active.finish();
        let shared = active.shared.clone();
        let (wav, writer_error) = match outcome {
            Some(o) => (o.wav, o.error),
            None => (
                None,
                Some("the recording thread stopped unexpectedly".into()),
            ),
        };
        let problem = problem.or(writer_error);
        let kept = |path: &Path| format!("The audio is kept in {}.", path.display());

        let Some(wav) = wav else {
            let why = problem.unwrap_or_else(|| "unknown error".into());
            self.notify(
                format!(
                    "The recording could not be saved ({why}). {}",
                    kept(&active.path)
                ),
                true,
            );
            return;
        };
        if wav.frames == 0 {
            let _ = std::fs::remove_file(&wav.path);
            if place {
                let why = problem
                    .map(|p| format!(" ({p})"))
                    .unwrap_or_else(|| " The transport never started or no audio arrived.".into());
                self.notify(format!("Nothing was recorded.{why}"), true);
            }
            return;
        }
        if !place {
            self.notify(format!("Recording discarded. {}", kept(&wav.path)), false);
            return;
        }
        let lanes: Vec<Id> = active
            .lanes
            .iter()
            .copied()
            .filter(|l| self.lane_exists(*l))
            .collect();
        if lanes.is_empty() {
            self.notify(
                format!(
                    "The armed lanes are gone, so no clip was added. {}",
                    kept(&wav.path)
                ),
                true,
            );
            return;
        }

        let tempo = TempoMap::from_segments(&self.doc.project().tempo);
        let start_beat = match shared.capture_start() {
            Some(start) => take_start_beat(&tempo, &start, f64::from(self.rec.offset_ms) / 1000.0),
            None => self.play_start,
        };
        let Some((start, length, offset)) = placement_for_take(&tempo, start_beat, wav.seconds())
        else {
            self.notify(
                format!(
                    "The recording ended before the song position it belongs to. {}",
                    kept(&wav.path)
                ),
                true,
            );
            return;
        };

        let name = format!(
            "Recording {}.wav",
            1 + self
                .doc
                .project()
                .assets
                .iter()
                .filter(|a| a.name.starts_with("Recording "))
                .count()
        );
        let (asset, import_warning) = self.recorded_asset(&wav, name);
        let asset_id = asset.id;
        let placements: Vec<Placement> = lanes
            .iter()
            .map(|&lane| Placement {
                lane,
                start,
                length,
                offset,
            })
            .collect();
        let ids = self.place_audio_clips("Record audio", Some(asset), asset_id, &placements);
        // Start decoding (and the waveform) now instead of at the next frame.
        self.sync();

        let dropped = shared.dropped_frames();
        let mut text = format!(
            "Recorded {:.1} s onto {} lane{}",
            wav.seconds(),
            ids.len(),
            if ids.len() == 1 { "" } else { "s" }
        );
        let mut error = false;
        if let Some(why) = problem {
            text.push_str(&format!(", but it stopped early: {why}"));
            error = true;
        }
        if dropped > 0 {
            text.push_str(&format!(
                ". The disk fell behind and {:.2} s of the take is silent",
                dropped as f64 / f64::from(wav.sample_rate.max(1))
            ));
            error = true;
        }
        if let Some(warning) = import_warning {
            text.push_str(&format!(". {warning}"));
            error = true;
        }
        self.notify(text, error);
        self.rec.finished = ids;
    }

    /// The asset for a finished take. A saved project gets a managed copy in
    /// its media folder; if that fails, or the project is unsaved, the asset
    /// references the file where it was written (the second value then says
    /// why, for the former).
    fn recorded_asset(&self, wav: &RecordedWav, name: String) -> (AudioAsset, Option<String>) {
        let mut warning = None;
        if let Some(project) = &self.path {
            let probe = AudioProbe {
                sample_rate: wav.sample_rate,
                channels: wav.channels,
                frames: wav.frames,
            };
            match peel_project::import_asset_with(project, &wav.path, |_| Some(probe)) {
                Ok(mut asset) => {
                    asset.name = name;
                    return (asset, None);
                }
                Err(e) => {
                    warning = Some(format!(
                        "It could not be copied into the project ({e}) and is referenced from {}",
                        wav.path.display()
                    ));
                }
            }
        }
        let hash = hash_file(&wav.path).unwrap_or_default();
        (
            AudioAsset {
                id: Id::new(),
                hash,
                name,
                path: wav.path.to_string_lossy().into_owned(),
                managed: false,
                sample_rate: wav.sample_rate,
                channels: wav.channels,
                frames: wav.frames,
            },
            warning,
        )
    }

    /// Posts `message` as an error notice and hands it back.
    fn refuse(&mut self, message: impl Into<String>) -> String {
        let message = message.into();
        self.notify(message.clone(), true);
        message
    }
}

#[cfg(test)]
#[path = "record_tests.rs"]
mod tests;
