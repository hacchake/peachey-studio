//! The authoritative musical document.
//!
//! Heavy collections (`Pattern`, `Arrangement`) sit behind `Arc` so that an
//! undo checkpoint is a cheap structural copy: editing a pattern clones only
//! that pattern (`Arc::make_mut`), while every untouched pattern is shared
//! between the live document and its history.

use std::collections::BTreeMap;
use std::sync::Arc;

use peel_time::{TempoSegment, Time, TimeSignature};
use serde::{Deserialize, Serialize};

use crate::{Color, Id};

pub const FORMAT: &str = "peel-project";
/// Schema used when a project does not need newer musical data.
pub const BASE_SCHEMA_VERSION: u32 = 1;
/// First schema that stores per-note expression without data loss in older readers.
pub const NOTE_EXPRESSION_SCHEMA: u32 = 2;
/// Newest project schema this reader supports.
pub const SCHEMA_VERSION: u32 = NOTE_EXPRESSION_SCHEMA;

/// Key used for a step that has not been given a pitch (C5 in Peel's
/// convention, MIDI 60).
pub const DEFAULT_ROOT_KEY: u8 = 60;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub format: String,
    pub schema: u32,
    pub id: Id,
    /// Incremented by every committed transaction.
    pub revision: u64,
    pub meta: ProjectMeta,
    /// Tempo map. The first segment is the song tempo.
    pub tempo: Vec<TempoSegment>,
    pub time_signature: TimeSignature,
    /// Global swing amount, 0..=1.
    pub swing: f32,
    /// Seed for probability and generators.
    pub seed: u64,
    pub channels: Vec<Channel>,
    pub patterns: Vec<Arc<Pattern>>,
    pub arrangements: Vec<Arc<Arrangement>>,
    pub active_arrangement: Id,
    pub mixer: Mixer,
    pub assets: Vec<AudioAsset>,
    /// Namespaced data from newer minor versions, preserved on save.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extensions: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ProjectMeta {
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub notes: String,
}

/// A sound or control generator in the shared rack. Not an arrangement lane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Channel {
    pub id: Id,
    pub name: String,
    pub color: Color,
    pub device: DeviceInstance,
    /// Destination mixer track.
    pub track: Id,
    pub gain_db: f32,
    pub pan: f32,
    pub muted: bool,
    /// Pitch used for new steps in the step sequencer.
    pub root_key: u8,
}

/// A native (or, later, external) device and its saved parameter values.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceInstance {
    pub id: Id,
    /// Stable device type, e.g. `peel.synth.poly`.
    pub type_id: String,
    /// DSP algorithm version the parameters were saved with.
    pub version: u32,
    /// Plain parameter values by stable parameter key.
    pub params: BTreeMap<String, f32>,
    /// Sample asset for sample-based devices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample: Option<Id>,
    #[serde(default)]
    pub bypass: bool,
    /// Optional user preset name shown in the device header.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// Opaque state of an external plugin (base64), captured on save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    /// Mixer track whose post-fader signal keys this device (a compressor
    /// ducking under the kick). Only for devices that accept a sidechain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sidechain: Option<Id>,
}

impl DeviceInstance {
    pub fn new(type_id: impl Into<String>, version: u32) -> DeviceInstance {
        DeviceInstance {
            id: Id::new(),
            type_id: type_id.into(),
            version,
            params: BTreeMap::new(),
            sample: None,
            bypass: false,
            preset: None,
            state: None,
            sidechain: None,
        }
    }
}

/// A reusable container of notes addressing any subset of channels.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pattern {
    pub id: Id,
    pub name: String,
    pub color: Color,
    /// Declared length in quarter notes; pattern mode loops this range.
    pub length: Time,
    /// All notes, for every channel. Kept sorted by (start, key).
    pub notes: Vec<Note>,
    /// Per-note performance events, addressed by the note's stable identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expression: Vec<NoteExpression>,
}

/// Maximum stored control points across the three curves of one note.
pub const MAX_EXPRESSION_POINTS: usize = 65_536;

/// One recorded control value at a note-relative quarter-note position.
/// Values hold until the next point; the instrument supplies its usual smoothing.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpressionPoint {
    pub offset: Time,
    pub value: f32,
}

/// Recorded performance curves belonging to one note in a pattern.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NoteExpression {
    pub note: Id,
    /// Pitch displacement in semitones, including any MPE manager bend.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pitch: Vec<ExpressionPoint>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pressure: Vec<ExpressionPoint>,
    /// Brightness/slide, 0..1 with neutral at 0.5.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub timbre: Vec<ExpressionPoint>,
}

/// Complete expression at a point in a note.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExpressionValue {
    pub pitch: f32,
    pub pressure: f32,
    pub timbre: f32,
}

impl Default for ExpressionValue {
    fn default() -> Self {
        Self {
            pitch: 0.0,
            pressure: 0.0,
            timbre: 0.5,
        }
    }
}

impl NoteExpression {
    pub fn at(&self, offset: Time) -> ExpressionValue {
        fn held(points: &[ExpressionPoint], offset: Time, neutral: f32) -> f32 {
            let end = points.partition_point(|p| p.offset <= offset);
            end.checked_sub(1).map_or(neutral, |i| points[i].value)
        }
        ExpressionValue {
            pitch: held(&self.pitch, offset, 0.0),
            pressure: held(&self.pressure, offset, 0.0),
            timbre: held(&self.timbre, offset, 0.5),
        }
    }

    /// Keeps the source interval, chasing its starting values to offset zero.
    pub fn slice(&self, note: Id, from: Time, length: Time) -> Self {
        fn slice_curve(
            points: &[ExpressionPoint],
            from: Time,
            length: Time,
            neutral: f32,
        ) -> Vec<ExpressionPoint> {
            if points.is_empty() {
                return Vec::new();
            }
            let mut out = Vec::new();
            let end = points.partition_point(|p| p.offset <= from);
            let value = end.checked_sub(1).map_or(neutral, |i| points[i].value);
            out.push(ExpressionPoint {
                offset: Time::ZERO,
                value,
            });
            out.extend(
                points[end..]
                    .iter()
                    .filter(|p| p.offset < from + length)
                    .map(|p| ExpressionPoint {
                        offset: p.offset - from,
                        value: p.value,
                    }),
            );
            out
        }
        Self {
            note,
            pitch: slice_curve(&self.pitch, from, length, 0.0),
            pressure: slice_curve(&self.pressure, from, length, 0.0),
            timbre: slice_curve(&self.timbre, from, length, 0.5),
        }
    }
}

impl Pattern {
    /// Removes expression for deleted notes and points beyond shortened notes.
    pub fn retain_expression(&mut self) {
        let lengths: std::collections::HashMap<_, _> =
            self.notes.iter().map(|n| (n.id, n.length)).collect();
        self.expression.retain_mut(|expression| {
            let Some(&length) = lengths.get(&expression.note) else {
                return false;
            };
            for points in [
                &mut expression.pitch,
                &mut expression.pressure,
                &mut expression.timbre,
            ] {
                points.retain(|p| p.offset >= Time::ZERO && p.offset <= length);
            }
            true
        });
    }

    pub fn expression_for(&self, note: Id) -> Option<&NoteExpression> {
        self.expression.iter().find(|e| e.note == note)
    }

    pub fn notes_for(&self, channel: Id) -> impl Iterator<Item = &Note> {
        self.notes.iter().filter(move |n| n.channel == channel)
    }

    pub fn uses_channel(&self, channel: Id) -> bool {
        self.notes.iter().any(|n| n.channel == channel)
    }

    pub fn sort_notes(&mut self) {
        self.notes.sort_by(|a, b| {
            a.start
                .cmp(&b.start)
                .then(a.key.cmp(&b.key))
                .then(a.id.cmp(&b.id))
        });
    }

    /// End of the last note, or zero.
    pub fn content_end(&self) -> Time {
        self.notes
            .iter()
            .map(|n| n.start + n.length)
            .max()
            .unwrap_or(Time::ZERO)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Note {
    pub id: Id,
    pub channel: Id,
    pub start: Time,
    pub length: Time,
    pub key: u8,
    /// Attack velocity 0..=1.
    pub velocity: f32,
    /// Release velocity 0..=1.
    #[serde(default = "default_release")]
    pub release: f32,
    /// Per-note pan -1..=1.
    #[serde(default)]
    pub pan: f32,
    /// Fine pitch in cents.
    #[serde(default)]
    pub fine: f32,
    #[serde(default)]
    pub muted: bool,
    /// Trigger probability 0..=1 (1 = always).
    #[serde(default = "default_probability")]
    pub probability: f32,
}

fn default_release() -> f32 {
    0.5
}

fn default_probability() -> f32 {
    1.0
}

impl Note {
    pub fn new(channel: Id, start: Time, length: Time, key: u8, velocity: f32) -> Note {
        Note {
            id: Id::new(),
            channel,
            start,
            length,
            key,
            velocity,
            release: 0.5,
            pan: 0.0,
            fine: 0.0,
            muted: false,
            probability: 1.0,
        }
    }

    pub fn end(&self) -> Time {
        self.start + self.length
    }
}

/// An arrangement: lanes holding clips. Alternate arrangements share the
/// underlying patterns and media.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Arrangement {
    pub id: Id,
    pub name: String,
    pub tracks: Vec<Track>,
    pub clips: Vec<Clip>,
    #[serde(default)]
    pub markers: Vec<Marker>,
    /// Half-open loop range in quarter notes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loop_range: Option<(Time, Time)>,
}

impl Arrangement {
    /// End of the last clip, or zero.
    pub fn end(&self) -> Time {
        self.clips
            .iter()
            .map(|c| c.start + c.length)
            .max()
            .unwrap_or(Time::ZERO)
    }
}

/// A playlist lane. Any clip type may be placed on any lane.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub id: Id,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<Color>,
    #[serde(default)]
    pub muted: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub id: Id,
    pub track: Id,
    /// Arrangement position.
    pub start: Time,
    /// Visible length. A pattern clip longer than its pattern loops it.
    pub length: Time,
    /// Offset into the source where playback begins.
    #[serde(default)]
    pub offset: Time,
    #[serde(default)]
    pub muted: bool,
    pub content: ClipContent,
}

impl Clip {
    pub fn end(&self) -> Time {
        self.start + self.length
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClipContent {
    /// A linked instance of a pattern source.
    Pattern { pattern: Id },
    /// Audio from an immutable asset, routed to a mixer track.
    ///
    /// The enclosing [`Clip`] places and trims it: `start` is where the
    /// audio sounds, `length` is how much of it plays, and `offset` is how
    /// far into the file (in quarter notes at the tempo in force at `start`)
    /// the first audible frame lies. Without `source_bpm` the audio plays at
    /// its native speed, so a tempo change moves the clip but never changes
    /// how fast it plays. With it, the audio is time-stretched (pitch kept)
    /// from that tempo to the song's tempo at the clip start.
    Audio {
        asset: Id,
        #[serde(default)]
        gain_db: f32,
        /// Play the visible window of the clip backwards.
        #[serde(default)]
        reversed: bool,
        track: Id,
        /// Fade-in length from the clip start, in quarter notes.
        #[serde(default, skip_serializing_if = "time_is_zero")]
        fade_in: Time,
        /// Fade-out length to the clip end, in quarter notes.
        #[serde(default, skip_serializing_if = "time_is_zero")]
        fade_out: Time,
        /// Tempo the audio was played at; when set, it follows the song.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source_bpm: Option<f64>,
    },
    /// An envelope that drives one parameter while the clip plays.
    Automation {
        target: AutomationTarget,
        /// Breakpoints sorted by position, relative to the clip's source
        /// (the clip `offset` applies as for patterns).
        points: Vec<AutomationPoint>,
    },
}

/// What an automation clip controls. Targets use stable identities: the
/// device instance id plus the parameter's stable key, never a position.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AutomationTarget {
    Param {
        device: Id,
        param: String,
    },
    TrackGain {
        track: Id,
    },
    TrackPan {
        track: Id,
    },
    /// A MIDI control change sent to an instrument while the clip plays.
    /// `value` 1.0 is controller value 127. Plugins receive it as MIDI;
    /// built-in instruments honour the sustain pedal (`cc` 64) and ignore
    /// other controllers. Hold a pedal with a step: two points at the same
    /// position.
    MidiCc {
        device: Id,
        cc: u8,
    },
}

/// One breakpoint. `value` is normalized 0..=1 across the target's range;
/// segments interpolate linearly to the next point.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutomationPoint {
    pub position: Time,
    pub value: f32,
    /// Bend of the segment to the next point, -1..=1 (0 is a straight
    /// line); see [`automation_curve`].
    #[serde(default, skip_serializing_if = "f32_is_zero")]
    pub tension: f32,
}

impl AutomationPoint {
    pub fn new(position: Time, value: f32) -> AutomationPoint {
        AutomationPoint {
            position,
            value,
            tension: 0.0,
        }
    }
}

fn f32_is_zero(v: &f32) -> bool {
    *v == 0.0
}

/// Shape of a segment: maps progress `t` (0..=1) through a segment to
/// progress in value. Positive tension starts slowly and finishes fast,
/// negative the reverse; both ends stay fixed.
pub fn automation_curve(t: f32, tension: f32) -> f32 {
    let k = tension.clamp(-1.0, 1.0);
    if k.abs() < 1e-4 {
        return t;
    }
    let e = 1.0 + k.abs() * 6.0;
    if k > 0.0 {
        t.powf(e)
    } else {
        1.0 - (1.0 - t).powf(e)
    }
}

/// Value of a breakpoint envelope at `pos` (holds outside the points).
pub fn automation_value(points: &[AutomationPoint], pos: Time) -> Option<f32> {
    let first = points.first()?;
    if pos <= first.position {
        return Some(first.value);
    }
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        if pos < b.position {
            let span = (b.position - a.position).as_f64();
            if span <= 0.0 {
                return Some(b.value);
            }
            let t = ((pos - a.position).as_f64() / span) as f32;
            return Some(a.value + (b.value - a.value) * automation_curve(t, a.tension));
        }
    }
    points.last().map(|p| p.value)
}

fn time_is_zero(t: &Time) -> bool {
    t.is_zero()
}

impl ClipContent {
    /// Audio clip content with no fades.
    pub fn audio(asset: Id, track: Id) -> ClipContent {
        ClipContent::Audio {
            asset,
            gain_db: 0.0,
            reversed: false,
            track,
            fade_in: Time::ZERO,
            fade_out: Time::ZERO,
            source_bpm: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Marker {
    pub id: Id,
    pub position: Time,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mixer {
    /// Mixer tracks. `tracks[0]` is always the master.
    pub tracks: Vec<MixerTrack>,
}

impl Mixer {
    pub fn master(&self) -> &MixerTrack {
        &self.tracks[0]
    }

    pub fn track(&self, id: Id) -> Option<&MixerTrack> {
        self.tracks.iter().find(|t| t.id == id)
    }

    pub fn track_mut(&mut self, id: Id) -> Option<&mut MixerTrack> {
        self.tracks.iter_mut().find(|t| t.id == id)
    }

    /// Tracks with an insert keyed from `source`: sidechain connections
    /// run from `source` to each of them.
    pub fn sidechain_targets(&self, source: Id) -> impl Iterator<Item = Id> + '_ {
        self.tracks
            .iter()
            .filter(move |t| t.inserts.iter().any(|d| d.sidechain == Some(source)))
            .map(|t| t.id)
    }

    /// Every track `id` feeds: its output, sends and sidechain keys.
    pub fn feeds(&self, id: Id) -> Vec<Id> {
        let Some(t) = self.track(id) else {
            return Vec::new();
        };
        let master = self.tracks[0].id;
        let mut v = Vec::new();
        if id != master {
            v.push(t.output.unwrap_or(master));
        }
        v.extend(t.sends.iter().map(|s| s.target));
        v.extend(self.sidechain_targets(id));
        v
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MixerTrack {
    pub id: Id,
    pub name: String,
    pub color: Color,
    pub gain_db: f32,
    pub pan: f32,
    pub muted: bool,
    pub solo: bool,
    /// Insert effects in processing order.
    pub inserts: Vec<DeviceInstance>,
    /// Destination track (master when `None`; ignored on the master).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Id>,
    /// Post-fader sends to other tracks (effect returns).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sends: Vec<MixerSend>,
}

/// A post-fader send to another mixer track.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MixerSend {
    pub target: Id,
    pub gain_db: f32,
}

/// An immutable audio file owned by or referenced from the project.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AudioAsset {
    pub id: Id,
    /// BLAKE3 hash of the original file bytes.
    pub hash: String,
    /// Original file name.
    pub name: String,
    /// Location: relative to the project's data directory when managed,
    /// absolute when referenced externally.
    pub path: String,
    #[serde(default)]
    pub managed: bool,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u64,
}

impl Project {
    pub fn channel(&self, id: Id) -> Option<&Channel> {
        self.channels.iter().find(|c| c.id == id)
    }

    pub fn channel_mut(&mut self, id: Id) -> Option<&mut Channel> {
        self.channels.iter_mut().find(|c| c.id == id)
    }

    pub fn pattern(&self, id: Id) -> Option<&Arc<Pattern>> {
        self.patterns.iter().find(|p| p.id == id)
    }

    /// Mutable access to a pattern, cloning it only if history shares it.
    pub fn pattern_mut(&mut self, id: Id) -> Option<&mut Pattern> {
        self.patterns
            .iter_mut()
            .find(|p| p.id == id)
            .map(Arc::make_mut)
    }

    pub fn arrangement(&self) -> &Arc<Arrangement> {
        self.arrangements
            .iter()
            .find(|a| a.id == self.active_arrangement)
            .unwrap_or(&self.arrangements[0])
    }

    pub fn arrangement_mut(&mut self) -> &mut Arrangement {
        let idx = self
            .arrangements
            .iter()
            .position(|a| a.id == self.active_arrangement)
            .unwrap_or(0);
        Arc::make_mut(&mut self.arrangements[idx])
    }

    pub fn asset(&self, id: Id) -> Option<&AudioAsset> {
        self.assets.iter().find(|a| a.id == id)
    }

    /// Song tempo (first tempo segment).
    pub fn bpm(&self) -> f64 {
        self.tempo.first().map_or(120.0, |s| s.bpm)
    }

    pub fn set_bpm(&mut self, bpm: f64) {
        let bpm = bpm.clamp(10.0, 999.0);
        match self.tempo.first_mut() {
            Some(s) => s.bpm = bpm,
            None => self.tempo.push(TempoSegment {
                start: Time::ZERO,
                bpm,
                ramp_to: None,
            }),
        }
    }

    /// Number of arrangement clips that reference a pattern.
    pub fn pattern_usage(&self, pattern: Id) -> usize {
        self.arrangements
            .iter()
            .flat_map(|a| a.clips.iter())
            .filter(|c| matches!(c.content, ClipContent::Pattern { pattern: p } if p == pattern))
            .count()
    }

    /// Next palette color not yet used by a channel, cycling when all are.
    pub fn next_channel_color(&self) -> Color {
        let n = self.channels.len();
        crate::color::PALETTE[n % crate::color::PALETTE.len()]
    }
}

/// Fader position (0..=1) to decibels: unity at 0.8, +6 dB at the top,
/// a curved scale down to -60 dB, and silence at 0. Shared by the mixer UI
/// and volume automation so both agree exactly.
pub fn fader_to_db(pos: f32) -> f32 {
    if pos <= 0.001 {
        return -96.0;
    }
    if pos >= 0.8 {
        (pos - 0.8) / 0.2 * 6.0
    } else {
        let x = pos / 0.8;
        -60.0 * (1.0 - x).powf(1.6)
    }
}

/// Inverse of [`fader_to_db`].
pub fn db_to_fader(db: f32) -> f32 {
    if db <= -95.0 {
        return 0.0;
    }
    if db >= 0.0 {
        0.8 + (db.min(6.0) / 6.0) * 0.2
    } else {
        let x = 1.0 - (-db / 60.0).min(1.0).powf(1.0 / 1.6);
        x * 0.8
    }
}
