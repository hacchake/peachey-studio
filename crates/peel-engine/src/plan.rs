//! The compiled, immutable execution snapshot of a document revision.
//!
//! The control thread builds a [`RenderPlan`] (allocating freely), sends it
//! to the engine, and receives the previous plan back through the retirement
//! queue. The engine never edits a plan's structure; realtime parameter
//! commands only adjust gains, pans and mutes in place.

use std::sync::Arc;

use peel_dsp::SampleBuffer;
use peel_time::TempoMap;

/// Index into [`RenderPlan::channels`].
pub type ChannelIndex = u16;
/// Index into [`RenderPlan::tracks`].
pub type TrackIndex = u16;
/// Stable engine device slot (instruments and effects).
pub type Slot = u32;

#[derive(Clone, Debug)]
pub struct RenderPlan {
    /// Document revision this plan was compiled from.
    pub revision: u64,
    pub tempo: TempoMap,
    /// Note events sorted by `(beat, order)`; ties keep note-offs before
    /// note-ons (see [`SeqEvent::order`]).
    pub sequence: Vec<SeqEvent>,
    /// Curved notes with prepared state for seeks and plan changes. Sorted by voice.
    pub expression_notes: Vec<ExpressionNote>,
    /// Balanced interval index, prepared off the callback before installation.
    pub expression_chase: Vec<ExpressionChaseNode>,
    /// Compiled source ids, sorted by voice, for matching notes across edits.
    pub note_identities: Vec<SourceNote>,
    /// The reverse index, prepared on the control thread and sorted by identity.
    pub note_identity_index: Vec<(NoteIdentity, u32)>,
    /// Musical end of the material (quarter notes). Song mode stops (or
    /// keeps running silently) after it; pattern mode loops at it.
    pub length_beats: f64,
    /// Half-open loop range in quarter notes. When set, the transport wraps
    /// from `end` to `start`, releasing sounding notes at the wrap.
    pub loop_range: Option<(f64, f64)>,
    /// Instrument channels.
    pub channels: Vec<ChannelRoute>,
    /// Audio file regions, in any order. Each plays into its own mixer
    /// track (see [`AudioRegion`]).
    pub audio: Vec<AudioRegion>,
    /// Parameter envelopes applied while playing.
    pub automation: Vec<AutomationLane>,
    /// Mixer tracks in processing order: every track appears after all of
    /// the tracks that feed it. The master is `tracks[master]`.
    pub tracks: Vec<TrackRoute>,
    pub master: TrackIndex,
    /// Beats per bar, for the metronome accent.
    pub beats_per_bar: u32,
}

impl RenderPlan {
    /// Rebuilds the interval index for note-expression chase. Control thread only.
    pub fn prepare_expression_chase(&mut self) {
        let mut order: Vec<u32> = (0..self.expression_notes.len()).map(|i| i as u32).collect();
        order.sort_unstable_by(|&a, &b| {
            let a = &self.expression_notes[a as usize];
            let b = &self.expression_notes[b as usize];
            a.start_beat
                .total_cmp(&b.start_beat)
                .then(a.voice.cmp(&b.voice))
        });
        let mut nodes = std::mem::take(&mut self.expression_chase);
        nodes.clear();
        nodes.reserve(order.len());
        fn build(
            order: &[u32],
            notes: &[ExpressionNote],
            out: &mut Vec<ExpressionChaseNode>,
        ) -> Option<u32> {
            if order.is_empty() {
                return None;
            }
            let middle = order.len() / 2;
            let index = out.len() as u32;
            let note = order[middle];
            out.push(ExpressionChaseNode {
                note,
                left: None,
                right: None,
                max_end: notes[note as usize].end_beat,
            });
            let left = build(&order[..middle], notes, out);
            let right = build(&order[middle + 1..], notes, out);
            let mut max_end = notes[note as usize].end_beat;
            for child in [left, right].into_iter().flatten() {
                max_end = max_end.max(out[child as usize].max_end);
            }
            out[index as usize] = ExpressionChaseNode {
                note,
                left,
                right,
                max_end,
            };
            Some(index)
        }
        build(&order, &self.expression_notes, &mut nodes);
        self.expression_chase = nodes;
    }

    /// Sounding expression notes at a transport jump, without allocation.
    pub fn sounding_expression(&self, beat: f64) -> impl Iterator<Item = &ExpressionNote> {
        ExpressionCandidates {
            notes: &self.expression_notes,
            nodes: &self.expression_chase,
            beat,
            stack: [0; 64],
            count: usize::from(!self.expression_chase.is_empty()),
            // A valid balanced index needs at most twice its depth per result.
            // This also contains malformed raw indexes without risking a loop.
            remaining: crate::MAX_ACTIVE_NOTES * 128,
        }
    }

    /// A silent plan with only a master track.
    pub fn empty() -> RenderPlan {
        RenderPlan {
            revision: 0,
            tempo: TempoMap::constant(120.0),
            sequence: Vec::new(),
            expression_notes: Vec::new(),
            expression_chase: Vec::new(),
            note_identities: Vec::new(),
            note_identity_index: Vec::new(),
            length_beats: 4.0,
            loop_range: Some((0.0, 4.0)),
            channels: Vec::new(),
            audio: Vec::new(),
            automation: Vec::new(),
            tracks: vec![TrackRoute::new(None, 0)],
            master: 0,
            beats_per_bar: 4,
        }
    }
}

/// A stretch of decoded audio placed on the timeline.
///
/// The region sounds from `start_beat` to `end_beat`. Its position in time
/// comes from the plan's tempo map, but the audio itself always plays at its
/// native speed (there is no time stretching): a tempo change moves the
/// region's edges, never the pitch or speed of what is inside.
///
/// The engine derives everything from the transport's absolute sample anchor
/// every block, so seeking or looping into the middle of a region starts at
/// the right source frame, and the result does not depend on how the host
/// splits the stream.
#[derive(Clone, Debug)]
pub struct AudioRegion {
    /// Destination mixer track. Regions pointing past the plan's tracks are
    /// ignored.
    pub track: TrackIndex,
    /// Decoded audio. It should already be at the engine sample rate (then
    /// it plays bit for bit); any other rate is played with linear
    /// interpolation. The control side owns the allocation: the engine only
    /// borrows it, and hands every extra reference it takes back through the
    /// retirement queue.
    pub sample: Arc<SampleBuffer>,
    /// Musical position of the first audible frame (quarter notes).
    pub start_beat: f64,
    /// Musical position where the region ends (quarter notes, exclusive).
    pub end_beat: f64,
    /// Seconds into `sample` of the frame that sounds at `start_beat`.
    /// Negative values leave silence before the sample begins.
    pub source_start_seconds: f64,
    /// Linear gain.
    pub gain: f32,
    /// Play the window `[source_start, source_start + length)` backwards.
    pub reversed: bool,
    /// Linear fade from silence over this many quarter notes after
    /// `start_beat`.
    pub fade_in_beats: f64,
    /// Linear fade to silence over this many quarter notes before
    /// `end_beat`.
    pub fade_out_beats: f64,
}

impl PartialEq for AudioRegion {
    fn eq(&self, other: &AudioRegion) -> bool {
        self.track == other.track
            && Arc::ptr_eq(&self.sample, &other.sample)
            && self.start_beat == other.start_beat
            && self.end_beat == other.end_beat
            && self.source_start_seconds == other.source_start_seconds
            && self.gain == other.gain
            && self.reversed == other.reversed
            && self.fade_in_beats == other.fade_in_beats
            && self.fade_out_beats == other.fade_out_beats
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SeqEvent {
    /// Musical position in quarter notes from the plan origin.
    pub beat: f64,
    pub channel: ChannelIndex,
    /// Tie-break for equal beats: note-offs 0, note-ons 1, expression 2.
    pub order: u8,
    pub kind: SeqKind,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SeqKind {
    /// `voice` is unique per realized note (pattern instance x loop
    /// iteration x note), so overlapping instances never share note-offs.
    NoteOn {
        key: u8,
        velocity: f32,
        voice: u32,
    },
    NoteOff {
        key: u8,
        velocity: f32,
        voice: u32,
    },
    Expression {
        voice: u32,
        pitch: f32,
        pressure: f32,
        timbre: f32,
    },
}

/// A node in the control-prepared note interval index.
#[derive(Clone, Debug)]
pub struct ExpressionChaseNode {
    note: u32,
    left: Option<u32>,
    right: Option<u32>,
    max_end: f64,
}

struct ExpressionCandidates<'a> {
    notes: &'a [ExpressionNote],
    nodes: &'a [ExpressionChaseNode],
    beat: f64,
    stack: [u32; 64],
    count: usize,
    remaining: usize,
}

impl<'a> Iterator for ExpressionCandidates<'a> {
    type Item = &'a ExpressionNote;
    fn next(&mut self) -> Option<Self::Item> {
        while self.count > 0 && self.remaining > 0 {
            self.remaining -= 1;
            self.count -= 1;
            let Some(node) = self.nodes.get(self.stack[self.count] as usize) else {
                continue;
            };
            if node.max_end <= self.beat {
                continue;
            }
            let Some(note) = self.notes.get(node.note as usize) else {
                continue;
            };
            let right = if note.start_beat < self.beat {
                node.right
            } else {
                None
            };
            for child in [right, node.left].into_iter().flatten() {
                if self.count < self.stack.len() {
                    self.stack[self.count] = child;
                    self.count += 1;
                }
            }
            if note.start_beat < self.beat && note.end_beat > self.beat {
                return Some(note);
            }
        }
        None
    }
}

/// Exact source-note identity independent of a plan's event ordering.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NoteIdentity {
    pub note: u128,
    pub instance: u128,
    pub iteration: u64,
}

/// One realized note's stable identity and its compact id in this plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceNote {
    pub voice: u32,
    pub identity: NoteIdentity,
}

/// Expression snapshot at an absolute musical position.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExpressionState {
    pub beat: f64,
    pub pitch: f32,
    pub pressure: f32,
    pub timbre: f32,
}

/// Control-prepared note interval for bounded, allocation-free expression chase.
#[derive(Clone, Debug, PartialEq)]
pub struct ExpressionNote {
    pub start_beat: f64,
    pub end_beat: f64,
    pub channel: ChannelIndex,
    pub key: u8,
    pub velocity: f32,
    pub voice: u32,
    pub points: Vec<ExpressionState>,
}

/// One instrument channel: a device slot feeding a mixer track.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChannelRoute {
    pub slot: Slot,
    pub track: TrackIndex,
    /// Linear gain.
    pub gain: f32,
    /// -1..=1.
    pub pan: f32,
    pub muted: bool,
}

/// One mixer track (bus).
#[derive(Clone, Debug, PartialEq)]
pub struct TrackRoute {
    /// Insert effect slots in processing order.
    pub inserts: Vec<Slot>,
    /// Linear gain.
    pub gain: f32,
    pub pan: f32,
    /// Effective mute (includes solo logic resolved by the control side).
    pub muted: bool,
    /// Destination track; `None` only for the master.
    pub output: Option<TrackIndex>,
    /// Telemetry meter index (stable per mixer track identity).
    pub meter: u16,
    /// Post-fader sends to later tracks in plan order.
    pub sends: Vec<TrackSend>,
    /// Sidechain keys: an insert slot and the earlier track (in plan order)
    /// whose signal it listens to.
    pub keys: Vec<(Slot, TrackIndex)>,
    /// Another track's insert listens to this one: keep its pre-fader
    /// signal as a key every block.
    pub key_source: bool,
}

/// A post-fader send from one track into another.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackSend {
    pub dest: TrackIndex,
    /// Linear send level.
    pub gain: f32,
    /// Level used at the end of the previous block, for click-free ramps
    /// ([`AUTOMATION_UNSET`] until the engine first applies it).
    pub last: f32,
}

impl TrackRoute {
    pub fn new(output: Option<TrackIndex>, meter: u16) -> TrackRoute {
        TrackRoute {
            inserts: Vec::new(),
            gain: 1.0,
            pan: 0.0,
            muted: false,
            output,
            meter,
            sends: Vec::new(),
            keys: Vec::new(),
            key_source: false,
        }
    }
}

/// What an automation lane drives.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AutomationTarget {
    /// A device parameter (plain value).
    Param { slot: Slot, id: peel_dsp::ParamId },
    /// A mixer track's linear gain.
    TrackGain(TrackIndex),
    /// A mixer track's pan (-1..=1).
    TrackPan(TrackIndex),
    /// A MIDI control change sent to a device as events (value 0..=1).
    Controller { slot: Slot, cc: u8 },
}

/// Value the engine has not applied yet (forces the first application).
pub const AUTOMATION_UNSET: f32 = -1.0e30;

/// One automation clip, compiled to absolute beats and plain values.
#[derive(Clone, Debug, PartialEq)]
pub struct AutomationLane {
    pub target: AutomationTarget,
    /// Active range `[start, end)` in quarter notes.
    pub start: f64,
    pub end: f64,
    /// Breakpoints `(beat, plain value)`, sorted by beat. Values hold before
    /// the first and after the last point inside the active range.
    pub points: Vec<(f64, f32)>,
    /// Last value the engine applied ([`AUTOMATION_UNSET`] on the control
    /// side).
    pub last: f32,
}

impl AutomationLane {
    /// Envelope value at `beat`, or `None` outside the active range.
    pub fn value_at(&self, beat: f64) -> Option<f32> {
        if beat < self.start || beat >= self.end || self.points.is_empty() {
            return None;
        }
        let i = self.points.partition_point(|p| p.0 <= beat);
        if i == 0 {
            return Some(self.points[0].1);
        }
        if i >= self.points.len() {
            return Some(self.points[self.points.len() - 1].1);
        }
        let (b0, v0) = self.points[i - 1];
        let (b1, v1) = self.points[i];
        let span = b1 - b0;
        if span <= 0.0 {
            return Some(v1);
        }
        let t = ((beat - b0) / span) as f32;
        Some(v0 + (v1 - v0) * t)
    }
}
