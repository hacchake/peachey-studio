use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// Each variant becomes a discoverable MCP tool with its own input schema.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    /// Read the session status, transport, recording, undo history and export jobs.
    Status {},
    /// Read the project or a subtree using a JSON Pointer, e.g. /patterns/0/notes. Includes the revision for optimistic edits.
    ReadProject { pointer: Option<String> },
    /// Read the agent guide, including document editing and connection instructions.
    Help {},
    /// Generate stable UUIDs for objects added with edit_project. Count must be 1..=256.
    NewIds { count: usize },
    /// Discover native instruments, effects, installed CLAP and VST3 plugins, parameter ranges and factory presets. Filter by type_id for full details.
    Devices { type_id: Option<String> },
    /// Create an empty project, or load a built-in song: demo true loads the first demo song, and song names a demo song by title or a song starter by name (an unknown name is refused with the list of valid ones). Refuses to discard unsaved work unless discard_changes is true.
    NewProject {
        title: String,
        #[serde(default)]
        demo: bool,
        #[serde(default)]
        song: Option<String>,
        #[serde(default)]
        discard_changes: bool,
    },
    /// Open a .peelproj file. Refuses to discard unsaved work unless discard_changes is true.
    OpenProject {
        path: String,
        #[serde(default)]
        discard_changes: bool,
    },
    /// Save the project atomically. Omit path to use the current file; pass an absolute path for Save As.
    SaveProject { path: Option<String> },
    /// Apply an atomic RFC 6902 JSON Patch as one undo step. Supports every project field, including notes, routing, sends, automation, tempo, markers, alternate arrangements and plugin state. Read the current revision first; stale edits are rejected.
    EditProject {
        expected_project_id: String,
        expected_revision: u64,
        label: String,
        operations: json_patch::Patch,
    },
    /// Undo or redo one document transaction.
    History { direction: HistoryDirection },
    /// Add an instrument channel with its own mixer track. Returns channel, device and track IDs.
    AddChannel { name: String, type_id: String },
    /// Create an empty pattern. Musical times are strings of quarter notes, e.g. "4" or "1/3".
    AddPattern { name: String, length: String },
    /// Replace one channel's notes in a pattern as one undo step. Other channels' notes are preserved. Returns generated note IDs.
    SetNotes {
        pattern: String,
        channel: String,
        notes: Vec<NoteInput>,
    },
    /// Read or edit a note's saved pitch, pressure and timbre curves. Offsets are note-relative quarter-note strings; omitted curves stay unchanged, [] clears a curve. Recorded values hold until the next point.
    NoteExpression {
        pattern: String,
        note: String,
        pitch: Option<Vec<ExpressionPointInput>>,
        pressure: Option<Vec<ExpressionPointInput>>,
        timbre: Option<Vec<ExpressionPointInput>>,
    },
    /// Apply a compositional tool to a pattern or a selection of note IDs as one undo step. Supports legato, articulation, chop, arpeggiate, strum, flam, humanize, levels, flip and tidy.
    TransformNotes {
        pattern: String,
        selection: Option<Vec<String>>,
        tool: crate::compose::NoteTool,
    },
    /// Browse a local directory for projects, MIDI and audio files. Results are sorted, with directories first. Limit is 1..=1000.
    BrowseFiles {
        directory: String,
        offset: Option<usize>,
        limit: Option<usize>,
    },
    /// Rescan installed CLAP and VST3 plugins in the background. Poll status for plugins_scanning.
    RescanPlugins {},
    /// Open (default) or close a plugin instrument's or effect's own window. VST3 plugins on Windows; the window appears on the desktop next to the studio.
    PluginWindow { device: String, open: Option<bool> },
    /// Place a linked pattern clip in an arrangement lane. A length longer than the pattern repeats it.
    PlacePattern {
        pattern: String,
        lane: String,
        start: String,
        length: String,
    },
    /// Add an effect to a mixer track. Returns its device ID.
    AddEffect { track: String, type_id: String },
    /// Set plain parameter values on an instrument or effect, optionally starting from a named factory preset. Keys and ranges come from devices.
    SetDevice {
        device: String,
        preset: Option<String>,
        #[serde(default)]
        params: std::collections::BTreeMap<String, f32>,
    },
    /// Control playback, song/pattern mode, position, looping and metronome. Only provided options change.
    Transport {
        command: Option<TransportCommand>,
        mode: Option<Mode>,
        pattern: Option<String>,
        beat: Option<f64>,
        looping: Option<bool>,
        metronome: Option<bool>,
    },
    /// Audition or release a MIDI note on a channel. Key is 0..127 and velocity is 0..1.
    LiveNote {
        channel: String,
        key: u8,
        velocity: f32,
        on: bool,
    },
    /// Import a local audio file into a lane at the given quarter-note position.
    ImportAudio {
        path: String,
        lane: String,
        start: String,
    },
    /// Preview a local audio file, or stop the preview by omitting path.
    PreviewAudio { path: Option<String> },
    /// Render a pattern clip to audio in the background. Poll status for render_progress.
    BounceClip { clip: String },
    /// Import a Standard MIDI File into the current project as one undo step.
    ImportMidi {
        path: String,
        #[serde(default)]
        adopt_tempo: bool,
    },
    /// Export the active arrangement or a selected pattern as a Standard MIDI File.
    ExportMidi {
        path: String,
        pattern: Option<String>,
    },
    /// Start an asynchronous mix or stems export. Format comes from the path (.wav, .mp3, .flac, .aiff). Poll export_job for completion; the returned job ID is local to this MCP session.
    ExportAudio {
        path: String,
        #[serde(default)]
        stems: bool,
        sample_rate: Option<u32>,
        bits: Option<u16>,
        bitrate_kbps: Option<u16>,
        tail_seconds: Option<f32>,
        #[serde(default)]
        mono: bool,
        #[serde(default)]
        normalize: bool,
        dither: Option<bool>,
    },
    /// Read an export job's progress/result or request cancellation. Completed results remain available.
    ExportJob {
        job: u64,
        #[serde(default)]
        cancel: bool,
    },
    /// List hardware audio outputs, inputs and MIDI ports. Hardware is opened only by the explicit configuration/recording tools.
    AudioDevices {},
    /// Choose audio output and buffer size. Omit device for the system default. Restarts the audio engine.
    ConfigureAudio {
        device: Option<String>,
        buffer_frames: Option<u32>,
    },
    /// Choose a MIDI input by its stable port ID, or disable MIDI by omitting port.
    ConfigureMidi {
        port: Option<String>,
        channel: Option<String>,
    },
    /// Read primary output channels and all additional hardware routes, including stream health and clock correction.
    OutputRoutes {},
    /// Replace additional hardware outputs atomically (up to four). Empty routes closes them. Each source is master or a mixer track; channels are zero-based. Independent devices are drift-corrected monitor feeds.
    ConfigureOutputRoutes { routes: Vec<OutputRouteInput> },
    /// Map primary playback to a mono output or a stereo pair on the current audio device. Channels are zero-based.
    ConfigureOutputChannels { left: u16, right: Option<u16> },
    /// Read MIDI channel filter, controller mappings, Learn status and mapping errors.
    MidiControls {},
    /// Replace MIDI controller mappings and choose an input channel filter (0..15; omit for omni). Controller ranges use plain parameter values.
    ConfigureMidiControls {
        channel_filter: Option<u8>,
        mappings: Vec<MidiMappingInput>,
    },
    /// Learn the next accepted MIDI controller for a target. Omit target to cancel Learn.
    MidiLearn { target: Option<MidiTargetInput> },
    /// Send an internal MIDI controller event through the same filter and Learn/mapping path as hardware. Channel is 0..15, controller and value are 0..127.
    MidiControl {
        channel: u8,
        controller: u8,
        value: u8,
    },
    /// Read or configure MPE input on the selected instrument. Choose off, lower or upper; members is 1..15, bend ranges 0..96 semitones. Defaults: 15 members, 48 semitones per note, 2 for the zone. Configuration is transient to this session.
    Mpe {
        mode: Option<MpeModeInput>,
        members: Option<u8>,
        member_bend_range: Option<f32>,
        manager_bend_range: Option<f32>,
    },
    /// Send an internal MIDI event through the same MPE and channel-filter path as hardware. Channel is zero-based 0..15, data bytes 0..127, pitch bend -8192..8191.
    MidiInput {
        channel: u8,
        event: MidiMessageInput,
    },
    /// Read or send transient pitch bend and aftertouch on the live instrument. Bend is -1..1; pressure is 0..1; bend_range is 0..48 semitones. Omitted values stay unchanged. Reset centers expression without stopping notes or changing saved parameters.
    MidiExpression {
        bend: Option<f32>,
        pressure: Option<f32>,
        key_pressure: Option<Vec<KeyPressureInput>>,
        bend_range: Option<f32>,
        #[serde(default)]
        reset: bool,
    },
    /// Configure audio recording input and monitoring. Input channels are zero-based; omit right for mono. This may open the microphone.
    ConfigureRecording {
        device: Option<String>,
        left: u16,
        right: Option<u16>,
        #[serde(default)]
        monitoring: bool,
        monitor_gain: Option<f32>,
        offset_ms: Option<f32>,
        directory: Option<String>,
    },
    /// Close the audio input and stop monitoring. Stop recording first.
    CloseInput {},
    /// Arm or disarm an arrangement lane for audio recording.
    ArmLane { lane: String, armed: bool },
    /// Start, stop or cancel an audio take. Start requires an armed lane and a usable input.
    Recording { command: RecordingCommand },
    /// Inspect launchable performance clips and their current states.
    PerformanceSlots {},
    /// Launch a clip or scene, stop a lane or the performance, or enable capture into a new arrangement.
    Performance {
        command: PerformanceCommand,
        lane: Option<String>,
        clip: Option<String>,
        scene: Option<usize>,
        capture: Option<bool>,
    },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KeyPressureInput {
    pub key: u8,
    pub value: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputRouteInput {
    pub name: String,
    pub track: Option<String>,
    pub device: String,
    pub left: u16,
    pub right: Option<u16>,
    #[serde(default)]
    pub gain_db: f32,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MidiTargetInput {
    DeviceParam { device: String, key: String },
    TrackGain { track: String },
    TrackPan { track: String },
    ChannelGain { channel: String },
    ChannelPan { channel: String },
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MidiMappingInput {
    pub controller: u8,
    pub channel: Option<u8>,
    pub target: MidiTargetInput,
    pub min: f32,
    pub max: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NoteInput {
    pub start: String,
    pub length: String,
    pub key: u8,
    pub velocity: f32,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum HistoryDirection {
    Undo,
    Redo,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TransportCommand {
    Play,
    Stop,
    Panic,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Song,
    Pattern,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RecordingCommand {
    Start,
    Stop,
    Cancel,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PerformanceCommand {
    Launch,
    Scene,
    StopLane,
    Stop,
    Capture,
}

pub fn decode(name: &str, mut arguments: serde_json::Map<String, Value>) -> Result<Action, String> {
    if arguments.contains_key("action") {
        return Err("action is determined by the tool name".into());
    }
    arguments.insert(
        "action".into(),
        Value::String(name.strip_prefix("peel_").ok_or("Unknown tool")?.into()),
    );
    serde_json::from_value(Value::Object(arguments)).map_err(|e| e.to_string())
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExpressionPointInput {
    pub offset: String,
    pub value: f32,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MpeModeInput {
    Off,
    Lower,
    Upper,
}
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MidiMessageInput {
    NoteOn { key: u8, velocity: u8 },
    NoteOff { key: u8, velocity: u8 },
    PitchBend { value: i16 },
    ChannelPressure { value: u8 },
    PolyPressure { key: u8, value: u8 },
    ControlChange { controller: u8, value: u8 },
}
