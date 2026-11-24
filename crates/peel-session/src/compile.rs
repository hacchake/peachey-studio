//! Document to [`RenderPlan`] compilation.
//!
//! Pattern events are realized at absolute positions per pattern instance
//! and loop iteration (`a + (p - o + k*L)`), so repeated instances never
//! accumulate rounding drift. Every realized note gets a unique voice id.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use peel_document::{Clip, ClipContent, Id, Pattern, Project, Time};
use peel_dsp::SampleBuffer;
use peel_dsp::util::db_to_gain;
use peel_engine::{
    AudioRegion, ChannelRoute, ExpressionNote, ExpressionState, NoteIdentity, RenderPlan, SeqEvent,
    SeqKind, Slot, SourceNote, TrackRoute,
};
use peel_time::TempoMap;

/// What the transport plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayMode {
    /// Loop the selected pattern.
    Pattern(Id),
    /// Play the active arrangement.
    Song,
}

/// Index maps produced alongside a plan, used to address realtime
/// parameter commands.
#[derive(Clone, Debug, Default)]
pub struct PlanIndex {
    pub channels: HashMap<Id, u16>,
    pub tracks: HashMap<Id, u16>,
}

/// Inputs the compiler needs besides the document.
pub struct CompileContext<'a> {
    pub mode: PlayMode,
    /// Engine slot for each device instance id.
    pub slots: &'a HashMap<Id, Slot>,
    /// Stable meter index for each mixer track id.
    pub meters: &'a HashMap<Id, u16>,
    /// Decoded audio by asset id, already at the engine sample rate. Audio
    /// clips whose asset is missing here are left out of the plan; the
    /// caller loads them and compiles again.
    pub samples: &'a HashMap<Id, Arc<SampleBuffer>>,
}

pub fn compile(p: &Project, ctx: &CompileContext) -> (RenderPlan, PlanIndex) {
    let mut index = PlanIndex::default();

    // ---- mixer tracks in processing order (feeders before destinations) ----
    let master_id = p.mixer.tracks[0].id;
    let order = processing_order(p);
    for (pi, &ti) in order.iter().enumerate() {
        index.tracks.insert(p.mixer.tracks[ti].id, pi as u16);
    }
    let any_solo = p.mixer.tracks.iter().any(|t| t.solo);
    let audible = if any_solo {
        solo_audible(p)
    } else {
        HashSet::new()
    };
    let feeds_solo = |id: Id| audible.contains(&id);
    let tracks: Vec<TrackRoute> = order
        .iter()
        .map(|&ti| {
            let t = &p.mixer.tracks[ti];
            let is_master = ti == 0;
            let output = if is_master {
                None
            } else {
                Some(
                    index.tracks[&t
                        .output
                        .filter(|o| index.tracks.contains_key(o))
                        .unwrap_or(master_id)],
                )
            };
            let solo_muted = any_solo && !is_master && !feeds_solo(t.id);
            TrackRoute {
                inserts: t
                    .inserts
                    .iter()
                    .filter(|d| !d.bypass)
                    .filter_map(|d| ctx.slots.get(&d.id).copied())
                    .collect(),
                gain: db_to_gain(t.gain_db),
                pan: t.pan,
                muted: t.muted || solo_muted,
                output,
                // Unallocated tracks must never alias the master meter or cue source.
                meter: ctx.meters.get(&t.id).copied().unwrap_or(u16::MAX),
                key_source: p.mixer.sidechain_targets(t.id).next().is_some(),
                keys: t
                    .inserts
                    .iter()
                    .filter(|d| !d.bypass)
                    .filter_map(|d| {
                        let slot = *ctx.slots.get(&d.id)?;
                        let src = *index.tracks.get(&d.sidechain?)?;
                        Some((slot, src))
                    })
                    .collect(),
                sends: if is_master {
                    Vec::new()
                } else {
                    t.sends
                        .iter()
                        .filter_map(|send| {
                            let dest = *index.tracks.get(&send.target)?;
                            Some(peel_engine::TrackSend {
                                dest,
                                gain: db_to_gain(send.gain_db),
                                last: peel_engine::AUTOMATION_UNSET,
                            })
                        })
                        .collect()
                },
            }
        })
        .collect();

    // ---- channels ----
    let mut channels = Vec::with_capacity(p.channels.len());
    for c in &p.channels {
        let Some(&slot) = ctx.slots.get(&c.device.id) else {
            continue;
        };
        index.channels.insert(c.id, channels.len() as u16);
        channels.push(ChannelRoute {
            slot,
            track: index
                .tracks
                .get(&c.track)
                .copied()
                .unwrap_or(index.tracks[&master_id]),
            gain: db_to_gain(c.gain_db),
            pan: c.pan,
            muted: c.muted,
        });
    }

    let tempo = TempoMap::from_segments(&p.tempo);

    // ---- sequence ----
    let mut seq = Vec::new();
    let mut expression_notes = Vec::new();
    let mut note_identities = Vec::new();
    let mut audio = Vec::new();
    let mut automation = Vec::new();
    let mut voice: u32 = 1;
    let (length, loop_range) = match ctx.mode {
        PlayMode::Pattern(pid) => {
            let len = p.pattern(pid).map_or(Time::beats(4), |pt| pt.length);
            if let Some(pt) = p.pattern(pid) {
                realize(
                    p,
                    pt,
                    Time::ZERO,
                    Time::ZERO,
                    pt.length,
                    &index,
                    0,
                    0,
                    &mut voice,
                    &mut seq,
                    &mut expression_notes,
                    &mut note_identities,
                );
            }
            (len.as_f64(), Some((0.0, len.as_f64())))
        }
        PlayMode::Song => {
            let arr = p.arrangement();
            let muted_tracks: Vec<Id> = arr
                .tracks
                .iter()
                .filter(|t| t.muted)
                .map(|t| t.id)
                .collect();
            for clip in &arr.clips {
                if clip.muted || muted_tracks.contains(&clip.track) {
                    continue;
                }
                match &clip.content {
                    ClipContent::Pattern { pattern } => {
                        if let Some(pt) = p.pattern(*pattern) {
                            let salt = clip.id.short();
                            realize(
                                p,
                                pt,
                                clip.start,
                                clip.offset,
                                clip.length,
                                &index,
                                salt,
                                clip.id.as_u128(),
                                &mut voice,
                                &mut seq,
                                &mut expression_notes,
                                &mut note_identities,
                            );
                        }
                    }
                    ClipContent::Audio { .. } => {
                        if let Some(region) =
                            audio_region(clip, &tempo, &index, index.tracks[&master_id], ctx)
                        {
                            audio.push(region);
                        }
                    }
                    ClipContent::Automation { target, points } => {
                        if let Some(lane) = automation_lane(p, clip, target, points, &index, ctx) {
                            automation.push(lane);
                        }
                    }
                }
            }
            let bar = p.time_signature.bar_length();
            let end = arr.end().ceil_to(bar).max(bar);
            let lr = arr
                .loop_range
                .map(|(a, b)| (a.as_f64(), b.as_f64()))
                .unwrap_or((0.0, end.as_f64()));
            (end.as_f64(), Some(lr))
        }
    };
    seq.sort_by(|a: &SeqEvent, b: &SeqEvent| a.beat.total_cmp(&b.beat).then(a.order.cmp(&b.order)));

    let mut note_identity_index: Vec<_> = note_identities
        .iter()
        .map(|n: &SourceNote| (n.identity, n.voice))
        .collect();
    note_identity_index.sort_unstable_by_key(|(identity, _)| *identity);
    let mut plan = RenderPlan {
        revision: p.revision,
        tempo,
        sequence: seq,
        expression_notes,
        expression_chase: Vec::new(),
        note_identities,
        note_identity_index,
        length_beats: length,
        loop_range,
        channels,
        audio,
        automation,
        tracks,
        master: (order.len() - 1) as u16,
        beats_per_bar: (p.time_signature.bar_length().as_f64()
            / p.time_signature.beat_length().as_f64())
        .round()
        .max(1.0) as u32,
    };
    plan.prepare_expression_chase();
    (plan, index)
}

/// Seconds into its file at which an audio clip starts sounding.
///
/// The clip's `offset` is in quarter notes of the tempo at the clip start, so
/// with a constant tempo it is exactly `offset * 60 / bpm`. The audio plays at
/// its native speed from there, so frame `n` of the file (at sample rate `r`)
/// sounds `n / r - source_start` seconds after the clip start, whatever the
/// tempo does afterwards.
pub fn clip_source_start_seconds(tempo: &TempoMap, clip: &Clip) -> f64 {
    clip.offset.as_f64() * 60.0 / tempo.bpm_at(clip.start.as_f64())
}

/// The region an audio clip plays as, or `None` if the clip cannot sound yet
/// (its audio is not decoded) or at all (empty).
fn audio_region(
    clip: &Clip,
    tempo: &TempoMap,
    index: &PlanIndex,
    master: u16,
    ctx: &CompileContext,
) -> Option<AudioRegion> {
    let ClipContent::Audio {
        asset,
        gain_db,
        reversed,
        track,
        fade_in,
        fade_out,
        ..
    } = &clip.content
    else {
        return None;
    };
    // A clip that follows the song tempo plays a stretched copy.
    let key = stretch_ratio(tempo, clip).map_or(*asset, |r| stretched_key(*asset, r));
    let sample = ctx.samples.get(&key)?;
    if !clip.length.is_positive() {
        return None;
    }
    let start = clip.start.as_f64();
    let length = clip.length.as_f64();
    let fade_in = fade_in.as_f64().clamp(0.0, length);
    let fade_out = fade_out.as_f64().clamp(0.0, length - fade_in);
    Some(AudioRegion {
        // A clip routed to a mixer track that no longer exists plays on the
        // master.
        track: index.tracks.get(track).copied().unwrap_or(master),
        sample: sample.clone(),
        start_beat: start,
        end_beat: (clip.start + clip.length).as_f64(),
        source_start_seconds: clip_source_start_seconds(tempo, clip),
        gain: db_to_gain(*gain_db),
        reversed: *reversed,
        fade_in_beats: fade_in,
        fade_out_beats: fade_out,
    })
}

/// How much a tempo-following audio clip is stretched (output length over
/// input length): its source tempo over the song's tempo at the clip start,
/// to a thousandth. `None` when the clip plays at its native speed.
pub fn stretch_ratio(tempo: &TempoMap, clip: &peel_document::Clip) -> Option<f64> {
    let ClipContent::Audio {
        source_bpm: Some(src),
        ..
    } = &clip.content
    else {
        return None;
    };
    let song = tempo.bpm_at(clip.start.as_f64());
    let r = (src / song * 1000.0).round() / 1000.0;
    (r.is_finite() && r > 0.05 && (r - 1.0).abs() > 1e-6).then_some(r)
}

/// The key a stretched copy of `asset` is passed to [`compile`] under.
pub fn stretched_key(asset: Id, ratio: f64) -> Id {
    let r = (ratio * 1000.0).round() as u128;
    Id::from_u128(asset.as_u128() ^ r.wrapping_mul(0x9E37_79B9_7F4A_7C15_F39C_C060_5CED_C835) ^ 1)
}

/// Swing offset for a note start (delays off-beat 16ths).
fn swing_offset(start: Time, swing: f32) -> f64 {
    if swing <= 0.0 {
        return 0.0;
    }
    let sixteenth = Time::new(1, 4);
    let in_beat = start.rem_euclid(Time::new(1, 2));
    if in_beat == sixteenth {
        swing.clamp(0.0, 1.0) as f64 / 12.0
    } else {
        0.0
    }
}

/// Deterministic per-note, per-iteration probability decision.
fn passes_probability(seed: u64, note: Id, salt: u64, iteration: i128, prob: f32) -> bool {
    if prob >= 1.0 {
        return true;
    }
    if prob <= 0.0 {
        return false;
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (seed, note, salt, iteration).hash(&mut h);
    let v = (h.finish() >> 11) as f64 / (1u64 << 53) as f64;
    v < prob as f64
}

/// Realizes one pattern instance placed at `start` with source `offset`
/// and visible `length`, looping the pattern source.
#[allow(clippy::too_many_arguments)]
fn realize(
    p: &Project,
    pt: &Pattern,
    start: Time,
    offset: Time,
    length: Time,
    index: &PlanIndex,
    salt: u64,
    instance: u128,
    voice: &mut u32,
    out: &mut Vec<SeqEvent>,
    expressed: &mut Vec<ExpressionNote>,
    identities: &mut Vec<SourceNote>,
) {
    let l = pt.length;
    if !l.is_positive() || !length.is_positive() {
        return;
    }
    let end = start + length;
    let expression: HashMap<_, _> = pt.expression.iter().map(|e| (e.note, e)).collect();
    // First iteration whose content can reach the visible window.
    let first_k = offset.checked_div(l).map(|q| q.floor_int()).unwrap_or(0) - 1;
    let mut k = first_k.max(0);
    loop {
        let iter_origin = start - offset + l.mul_int(k as i64);
        if iter_origin >= end {
            break;
        }
        for n in &pt.notes {
            if n.muted {
                continue;
            }
            let Some(&ch) = index.channels.get(&n.channel) else {
                continue;
            };
            // Notes past the source loop length are not played.
            if n.start >= l {
                continue;
            }
            let mut on = iter_origin + n.start;
            let note_end = (n.start + n.length).min(l);
            let off = (iter_origin + note_end).min(end);
            // Chase: a note that began before the clip's visible start but
            // is still sounding there starts at the clip start (the spec's
            // default clip-start policy), so trimmed and sliced clips keep
            // sounding.
            let chased = on < start && off > start;
            if chased {
                on = start;
            } else if on < start || on >= end {
                continue;
            }
            if !passes_probability(p.seed, n.id, salt, k, n.probability) {
                continue;
            }
            if off <= on {
                continue;
            }
            let sw = if chased {
                0.0
            } else {
                swing_offset(n.start, p.swing)
            };
            // The upper namespace belongs to live/MPE notes.
            if *voice >= 0x4000_0000 {
                return;
            }
            let v = *voice;
            *voice += 1;
            identities.push(SourceNote {
                voice: v,
                identity: NoteIdentity {
                    note: n.id.as_u128(),
                    instance,
                    iteration: k as u64,
                },
            });
            out.push(SeqEvent {
                beat: on.as_f64() + sw,
                channel: ch,
                order: 1,
                kind: SeqKind::NoteOn {
                    key: n.key,
                    velocity: n.velocity,
                    voice: v,
                },
            });
            let off_beat = off.as_f64() + sw.min((off - on).as_f64() * 0.5);
            if let Some(expression) = expression.get(&n.id) {
                let from = on - (iter_origin + n.start);
                let mut offsets = vec![from];
                offsets.extend(
                    expression
                        .pitch
                        .iter()
                        .chain(&expression.pressure)
                        .chain(&expression.timbre)
                        .map(|p| p.offset)
                        .filter(|&offset| offset > from && offset < n.length),
                );
                offsets.sort();
                offsets.dedup();
                let mut points = Vec::new();
                for offset in offsets {
                    let beat = (iter_origin + n.start + offset).as_f64() + sw;
                    if beat >= off_beat {
                        break;
                    }
                    let state = expression.at(offset);
                    out.push(SeqEvent {
                        beat,
                        channel: ch,
                        order: 2,
                        kind: SeqKind::Expression {
                            voice: v,
                            pitch: state.pitch,
                            pressure: state.pressure,
                            timbre: state.timbre,
                        },
                    });
                    points.push(ExpressionState {
                        beat,
                        pitch: state.pitch,
                        pressure: state.pressure,
                        timbre: state.timbre,
                    });
                }
                expressed.push(ExpressionNote {
                    start_beat: on.as_f64() + sw,
                    end_beat: off_beat,
                    channel: ch,
                    key: n.key,
                    velocity: n.velocity,
                    voice: v,
                    points,
                });
            }
            out.push(SeqEvent {
                beat: off_beat,
                channel: ch,
                order: 0,
                kind: SeqKind::NoteOff {
                    key: n.key,
                    velocity: n.release,
                    voice: v,
                },
            });
        }
        k += 1;
        if k > 1_000_000 {
            break;
        }
    }
}

/// True if two plans differ only in gains, pans and mutes, so realtime
/// commands can update the running plan without a swap.
pub fn same_structure(a: &RenderPlan, b: &RenderPlan) -> bool {
    a.sequence == b.sequence
        && a.expression_notes == b.expression_notes
        && a.note_identities == b.note_identities
        && a.note_identity_index == b.note_identity_index
        && a.audio == b.audio
        && a.automation == b.automation
        && a.tempo == b.tempo
        && a.length_beats == b.length_beats
        && a.loop_range == b.loop_range
        && a.master == b.master
        && a.beats_per_bar == b.beats_per_bar
        && a.channels.len() == b.channels.len()
        && a.tracks.len() == b.tracks.len()
        && a.channels
            .iter()
            .zip(&b.channels)
            .all(|(x, y)| x.slot == y.slot && x.track == y.track)
        && a.tracks.iter().zip(&b.tracks).all(|(x, y)| {
            x.inserts == y.inserts
                && x.keys == y.keys
                && x.key_source == y.key_source
                && x.output == y.output
                && x.meter == y.meter
                && x.sends.len() == y.sends.len()
                && x.sends.iter().zip(&y.sends).all(|(m, n)| m.dest == n.dest)
        })
}

/// Tracks heard while something is soloed: each soloed track, everything
/// that feeds it, and everything it feeds through outputs and sends (buses,
/// returns, the master). Other tracks are muted at their own faders, so an
/// unrelated source cannot leak through a shared bus or return. Sidechain
/// keys are not audible connections and keep working regardless.
fn solo_audible(p: &Project) -> HashSet<Id> {
    let audible_feeds = |id: Id| -> Vec<Id> {
        let Some(t) = p.mixer.track(id) else {
            return Vec::new();
        };
        let mut v = Vec::new();
        if id != p.mixer.tracks[0].id {
            v.push(t.output.unwrap_or(p.mixer.tracks[0].id));
        }
        v.extend(t.sends.iter().map(|s| s.target));
        v
    };
    let mut out = HashSet::new();
    for s in p.mixer.tracks.iter().filter(|t| t.solo) {
        // Downstream.
        let mut stack = vec![s.id];
        while let Some(c) = stack.pop() {
            if out.insert(c) {
                stack.extend(audible_feeds(c));
            }
        }
        // Upstream: every track whose audible path reaches the soloed one.
        for t in &p.mixer.tracks {
            let mut stack = vec![t.id];
            let mut seen = HashSet::new();
            while let Some(c) = stack.pop() {
                if c == s.id {
                    out.insert(t.id);
                    break;
                }
                if seen.insert(c) {
                    stack.extend(audible_feeds(c));
                }
            }
        }
    }
    out
}

/// Mixer tracks (indices into `p.mixer.tracks`) ordered so every track
/// comes after everything that feeds it through its output, a send or a
/// sidechain key; the
/// master is last. Tracks caught in a cycle (rejected by validation) are
/// appended in document order and their backward routes are ignored by the
/// engine.
fn processing_order(p: &Project) -> Vec<usize> {
    let n = p.mixer.tracks.len();
    let pos = |id: Id| p.mixer.tracks.iter().position(|t| t.id == id);
    // Edges source -> destination.
    let mut indegree = vec![0usize; n];
    let mut edges: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, t) in p.mixer.tracks.iter().enumerate() {
        // Outputs, sends and sidechain keys all run source -> destination.
        let dests: Vec<usize> = p.mixer.feeds(t.id).into_iter().filter_map(pos).collect();
        for d in dests {
            if d != i {
                edges[i].push(d);
                indegree[d] += 1;
            }
        }
    }
    let mut ready: Vec<usize> = (1..n).filter(|&i| indegree[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = ready.first().copied() {
        ready.remove(0);
        order.push(i);
        for &d in &edges[i] {
            indegree[d] -= 1;
            if indegree[d] == 0 && d != 0 {
                ready.push(d);
            }
        }
    }
    for i in 1..n {
        if !order.contains(&i) {
            order.push(i);
        }
    }
    order.push(0);
    order
}

/// Compiles one automation clip into an engine lane: breakpoints move to
/// absolute beats and normalized values become the target's plain values.
fn automation_lane(
    p: &Project,
    clip: &peel_document::Clip,
    target: &peel_document::AutomationTarget,
    points: &[peel_document::AutomationPoint],
    index: &PlanIndex,
    ctx: &CompileContext,
) -> Option<peel_engine::AutomationLane> {
    use peel_document::AutomationTarget as T;
    use peel_engine::AutomationTarget as E;
    let (engine_target, convert): (E, Box<dyn Fn(f32) -> f32>) = match target {
        T::Param { device, param } => {
            let inst = crate::find_device(p, *device)?;
            let desc = peel_devices::descriptor(&inst.type_id)?.param_by_key(param)?;
            let slot = *ctx.slots.get(device)?;
            (
                E::Param { slot, id: desc.id },
                Box::new(move |v| desc.from_normalized(v)),
            )
        }
        T::TrackGain { track } => {
            let t = *index.tracks.get(track)?;
            (
                E::TrackGain(t),
                Box::new(|v| db_to_gain(peel_document::fader_to_db(v))),
            )
        }
        T::TrackPan { track } => {
            let t = *index.tracks.get(track)?;
            (
                E::TrackPan(t),
                Box::new(|v| (v * 2.0 - 1.0).clamp(-1.0, 1.0)),
            )
        }
        T::MidiCc { device, cc } => {
            let slot = *ctx.slots.get(device)?;
            (
                E::Controller {
                    slot,
                    cc: (*cc).min(127),
                },
                Box::new(|v| v),
            )
        }
    };
    if points.is_empty() {
        return None;
    }
    let origin = clip.start - clip.offset;
    Some(peel_engine::AutomationLane {
        target: engine_target,
        start: clip.start.as_f64(),
        end: clip.end().as_f64(),
        points: {
            // The engine interpolates straight lines; curved segments are
            // sent as short straight pieces.
            const PIECES: usize = 16;
            let mut out = Vec::with_capacity(points.len() * 2);
            for (i, pt) in points.iter().enumerate() {
                let at = (origin + pt.position).as_f64();
                out.push((at, convert(pt.value.clamp(0.0, 1.0))));
                if let Some(next) = points.get(i + 1)
                    && pt.tension != 0.0
                    && next.value != pt.value
                {
                    let end = (origin + next.position).as_f64();
                    for k in 1..PIECES {
                        let f = k as f32 / PIECES as f32;
                        let v = pt.value
                            + (next.value - pt.value)
                                * peel_document::automation_curve(f, pt.tension);
                        out.push((at + (end - at) * f as f64, convert(v.clamp(0.0, 1.0))));
                    }
                }
            }
            out
        },
        last: peel_engine::AUTOMATION_UNSET,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use peel_document::{Arrangement, Channel, Clip, DeviceInstance, MixerTrack, Note, PALETTE};
    use std::sync::Arc;

    fn project() -> (Project, Id, Id) {
        let mut p = Project::empty("t");
        let tr = MixerTrack {
            id: Id::new(),
            name: "A".into(),
            color: PALETTE[0],
            gain_db: 0.0,
            pan: 0.0,
            muted: false,
            solo: false,
            inserts: vec![],
            output: None,
            sends: Vec::new(),
        };
        let ch = Channel {
            id: Id::new(),
            name: "Kick".into(),
            color: PALETTE[0],
            device: DeviceInstance::new("peel.drum.kick", 1),
            track: tr.id,
            gain_db: 0.0,
            pan: 0.0,
            muted: false,
            root_key: 60,
        };
        let cid = ch.id;
        p.mixer.tracks.push(tr);
        p.channels.push(ch);
        let pid = p.patterns[0].id;
        let pat = Arc::make_mut(&mut p.patterns[0]);
        for i in 0..4 {
            pat.notes
                .push(Note::new(cid, Time::beats(i), Time::new(1, 4), 60, 1.0));
        }
        (p, cid, pid)
    }

    fn ctx_for(p: &Project) -> (HashMap<Id, Slot>, HashMap<Id, u16>) {
        let slots = p
            .channels
            .iter()
            .enumerate()
            .map(|(i, c)| (c.device.id, i as u32))
            .collect();
        let meters = p
            .mixer
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| (t.id, i as u16))
            .collect();
        (slots, meters)
    }

    #[test]
    fn pattern_mode_loops_pattern() {
        let (p, _, pid) = project();
        let (slots, meters) = ctx_for(&p);
        let samples = HashMap::new();
        let (plan, _) = compile(
            &p,
            &CompileContext {
                mode: PlayMode::Pattern(pid),
                slots: &slots,
                meters: &meters,
                samples: &samples,
            },
        );
        assert_eq!(plan.sequence.len(), 8);
        assert_eq!(plan.loop_range, Some((0.0, 4.0)));
        assert_eq!(plan.tracks.len(), 2);
        assert_eq!(plan.master, 1);
        assert_eq!(plan.channels[0].track, 0);
    }

    #[test]
    fn song_mode_repeats_long_clips_without_drift() {
        let (mut p, _, pid) = project();
        let lane = p.arrangement().tracks[0].id;
        let arr: &mut Arrangement = p.arrangement_mut();
        arr.clips.push(Clip {
            id: Id::new(),
            track: lane,
            start: Time::beats(8),
            length: Time::beats(400),
            offset: Time::ZERO,
            muted: false,
            content: ClipContent::Pattern { pattern: pid },
        });
        let (slots, meters) = ctx_for(&p);
        let samples = HashMap::new();
        let (plan, _) = compile(
            &p,
            &CompileContext {
                mode: PlayMode::Song,
                slots: &slots,
                meters: &meters,
                samples: &samples,
            },
        );
        let ons: Vec<f64> = plan
            .sequence
            .iter()
            .filter(|e| matches!(e.kind, SeqKind::NoteOn { .. }))
            .map(|e| e.beat)
            .collect();
        assert_eq!(ons.len(), 400);
        assert_eq!(ons[0], 8.0);
        assert_eq!(*ons.last().unwrap(), 407.0);
        // Unique voices.
        let mut voices: Vec<u32> = plan
            .sequence
            .iter()
            .filter_map(|e| {
                if let SeqKind::NoteOn { voice, .. } = e.kind {
                    Some(voice)
                } else {
                    None
                }
            })
            .collect();
        voices.sort();
        voices.dedup();
        assert_eq!(voices.len(), 400);
    }

    #[test]
    fn clip_offset_and_trim() {
        let (mut p, _, pid) = project();
        let lane = p.arrangement().tracks[0].id;
        p.arrangement_mut().clips.push(Clip {
            id: Id::new(),
            track: lane,
            start: Time::ZERO,
            length: Time::new(5, 2),
            offset: Time::beats(1),
            muted: false,
            content: ClipContent::Pattern { pattern: pid },
        });
        let (slots, meters) = ctx_for(&p);
        let samples = HashMap::new();
        let (plan, _) = compile(
            &p,
            &CompileContext {
                mode: PlayMode::Song,
                slots: &slots,
                meters: &meters,
                samples: &samples,
            },
        );
        let ons: Vec<f64> = plan
            .sequence
            .iter()
            .filter(|e| matches!(e.kind, SeqKind::NoteOn { .. }))
            .map(|e| e.beat)
            .collect();
        // Source beats 1, 2, 3 land at 0, 1, 2; the clip ends at 2.5.
        assert_eq!(ons, vec![0.0, 1.0, 2.0]);
    }

    #[test]
    fn structure_comparison_ignores_gains() {
        let (mut p, _, pid) = project();
        let (slots, meters) = ctx_for(&p);
        let samples = HashMap::new();
        let ctx = CompileContext {
            mode: PlayMode::Pattern(pid),
            slots: &slots,
            meters: &meters,
            samples: &samples,
        };
        let (a, _) = compile(&p, &ctx);
        p.mixer.tracks[1].gain_db = -6.0;
        let (b, _) = compile(&p, &ctx);
        assert!(same_structure(&a, &b));
        assert!(a.tracks[0].gain != b.tracks[0].gain);
    }

    #[test]
    fn solo_mutes_other_tracks() {
        let (mut p, _, pid) = project();
        let other = MixerTrack {
            id: Id::new(),
            name: "B".into(),
            color: PALETTE[1],
            gain_db: 0.0,
            pan: 0.0,
            muted: false,
            solo: true,
            inserts: vec![],
            output: None,
            sends: Vec::new(),
        };
        p.mixer.tracks.push(other);
        let (slots, meters) = ctx_for(&p);
        let samples = HashMap::new();
        let (plan, index) = compile(
            &p,
            &CompileContext {
                mode: PlayMode::Pattern(pid),
                slots: &slots,
                meters: &meters,
                samples: &samples,
            },
        );
        let a = index.tracks[&p.mixer.tracks[1].id] as usize;
        let b = index.tracks[&p.mixer.tracks[2].id] as usize;
        assert!(plan.tracks[a].muted);
        assert!(!plan.tracks[b].muted);
        assert!(!plan.tracks[plan.master as usize].muted);
    }

    // ---- audio clips ----

    struct AudioFixture {
        p: Project,
        asset: Id,
        track: Id,
        lane: Id,
        samples: HashMap<Id, Arc<SampleBuffer>>,
    }

    fn audio_fixture() -> AudioFixture {
        let (mut p, _, _) = project();
        let asset = Id::new();
        p.assets.push(peel_document::AudioAsset {
            id: asset,
            hash: "00".into(),
            name: "loop.wav".into(),
            path: "loop.wav".into(),
            managed: false,
            sample_rate: 48_000,
            channels: 1,
            frames: 96_000,
        });
        let track = p.mixer.tracks[1].id;
        let lane = p.arrangement().tracks[0].id;
        let buf = Arc::new(SampleBuffer::new("loop", 48_000.0, vec![vec![0.5; 96_000]]));
        AudioFixture {
            p,
            asset,
            track,
            lane,
            samples: HashMap::from([(asset, buf)]),
        }
    }

    impl AudioFixture {
        fn clip(&mut self, start: Time, length: Time, offset: Time) -> &mut Clip {
            let clips = &mut self.p.arrangement_mut().clips;
            clips.push(Clip {
                id: Id::new(),
                track: self.lane,
                start,
                length,
                offset,
                muted: false,
                content: ClipContent::audio(self.asset, self.track),
            });
            clips.last_mut().unwrap()
        }

        fn compile(&self, mode: PlayMode) -> (RenderPlan, PlanIndex) {
            let (slots, meters) = ctx_for(&self.p);
            compile(
                &self.p,
                &CompileContext {
                    mode,
                    slots: &slots,
                    meters: &meters,
                    samples: &self.samples,
                },
            )
        }
    }

    #[test]
    fn audio_clips_become_regions_in_song_mode() {
        let mut f = audio_fixture();
        let (asset, track) = (f.asset, f.track);
        let c = f.clip(Time::beats(4), Time::beats(6), Time::new(3, 2));
        c.content = ClipContent::Audio {
            asset,
            gain_db: -6.0,
            reversed: true,
            track,
            fade_in: Time::new(1, 2),
            fade_out: Time::beats(1),
            source_bpm: None,
        };
        let (plan, index) = f.compile(PlayMode::Song);
        assert_eq!(plan.audio.len(), 1);
        let r = &plan.audio[0];
        assert_eq!(r.track, index.tracks[&track]);
        assert!(Arc::ptr_eq(&r.sample, &f.samples[&asset]));
        assert_eq!((r.start_beat, r.end_beat), (4.0, 10.0));
        // 1.5 beats at 120 BPM.
        assert_eq!(r.source_start_seconds, 0.75);
        assert!((r.gain - db_to_gain(-6.0)).abs() < 1e-7);
        assert!(r.reversed);
        assert_eq!((r.fade_in_beats, r.fade_out_beats), (0.5, 1.0));
        // The arrangement ends at beat 10; the plan runs to the next bar.
        assert_eq!(plan.length_beats, 12.0);

        // Pattern mode never plays arrangement audio.
        let (plan, _) = f.compile(PlayMode::Pattern(f.p.patterns[0].id));
        assert!(plan.audio.is_empty());
    }

    #[test]
    fn audio_clips_without_decoded_audio_are_skipped_until_it_arrives() {
        let mut f = audio_fixture();
        f.clip(Time::ZERO, Time::beats(4), Time::ZERO);
        let loaded = f.samples.clone();
        f.samples.clear();
        let (plan, _) = f.compile(PlayMode::Song);
        assert!(plan.audio.is_empty());
        // The length still counts, so the transport does not stop early.
        assert_eq!(plan.length_beats, 4.0);
        f.samples = loaded;
        assert_eq!(f.compile(PlayMode::Song).0.audio.len(), 1);
    }

    #[test]
    fn muted_clips_and_lanes_leave_no_region() {
        let mut f = audio_fixture();
        f.clip(Time::ZERO, Time::beats(2), Time::ZERO).muted = true;
        assert!(f.compile(PlayMode::Song).0.audio.is_empty());

        let mut f = audio_fixture();
        f.clip(Time::ZERO, Time::beats(2), Time::ZERO);
        let lane = f.lane;
        f.p.arrangement_mut()
            .tracks
            .iter_mut()
            .find(|t| t.id == lane)
            .unwrap()
            .muted = true;
        assert!(f.compile(PlayMode::Song).0.audio.is_empty());
    }

    #[test]
    fn fades_are_limited_to_the_clip() {
        let mut f = audio_fixture();
        let (asset, track) = (f.asset, f.track);
        let c = f.clip(Time::ZERO, Time::beats(2), Time::ZERO);
        c.content = ClipContent::Audio {
            asset,
            gain_db: 0.0,
            reversed: false,
            track,
            fade_in: Time::beats(3),
            fade_out: Time::beats(3),
            source_bpm: None,
        };
        let r = &f.compile(PlayMode::Song).0.audio[0];
        assert_eq!(r.fade_in_beats, 2.0);
        assert_eq!(r.fade_out_beats, 0.0);
    }

    #[test]
    fn audio_for_a_missing_mixer_track_plays_on_the_master() {
        let mut f = audio_fixture();
        let asset = f.asset;
        let c = f.clip(Time::ZERO, Time::beats(2), Time::ZERO);
        c.content = ClipContent::audio(asset, Id::new());
        let (plan, _) = f.compile(PlayMode::Song);
        assert_eq!(plan.audio[0].track, plan.master);
    }

    #[test]
    fn the_source_offset_follows_the_tempo_at_the_clip_start() {
        let mut f = audio_fixture();
        f.p.tempo = vec![
            peel_document::TempoSegment {
                start: Time::ZERO,
                bpm: 120.0,
                ramp_to: None,
            },
            peel_document::TempoSegment {
                start: Time::beats(8),
                bpm: 60.0,
                ramp_to: None,
            },
        ];
        f.clip(Time::beats(8), Time::beats(4), Time::beats(2));
        let r = &f.compile(PlayMode::Song).0.audio[0];
        assert_eq!(r.source_start_seconds, 2.0);
    }

    #[test]
    fn plans_with_different_audio_are_structurally_different() {
        let mut f = audio_fixture();
        f.clip(Time::ZERO, Time::beats(2), Time::ZERO);
        let (a, _) = f.compile(PlayMode::Song);
        let (b, _) = f.compile(PlayMode::Song);
        assert!(same_structure(&a, &b));
        f.p.arrangement_mut().clips[0].start = Time::beats(1);
        let (moved, _) = f.compile(PlayMode::Song);
        assert!(!same_structure(&a, &moved));
        let asset = f.asset;
        f.p.arrangement_mut().clips[0].content = ClipContent::Audio {
            asset,
            gain_db: -3.0,
            reversed: false,
            track: f.track,
            fade_in: Time::ZERO,
            fade_out: Time::ZERO,
            source_bpm: None,
        };
        f.p.arrangement_mut().clips[0].start = Time::ZERO;
        let (quieter, _) = f.compile(PlayMode::Song);
        assert!(!same_structure(&a, &quieter));
    }

    #[test]
    fn sends_compile_forward_and_loops_are_rejected() {
        let (mut p, _, pid) = project();
        let bus = MixerTrack {
            id: Id::new(),
            name: "Bus".into(),
            color: PALETTE[2],
            gain_db: 0.0,
            pan: 0.0,
            muted: false,
            solo: false,
            inserts: vec![],
            output: None,
            sends: vec![],
        };
        let bus_id = bus.id;
        // Insert the bus before the source so document order is "wrong".
        p.mixer.tracks.insert(1, bus);
        let src = p.mixer.tracks[2].id;
        p.mixer.tracks[2].sends.push(peel_document::MixerSend {
            target: bus_id,
            gain_db: -6.0,
        });
        assert!(peel_document::validate(&p).iter().all(|i| !i.is_error()));
        let (slots, meters) = ctx_for(&p);
        let (plan, index) = compile(
            &p,
            &CompileContext {
                mode: PlayMode::Pattern(pid),
                slots: &slots,
                meters: &meters,
                samples: &HashMap::new(),
            },
        );
        let (si, bi) = (index.tracks[&src] as usize, index.tracks[&bus_id] as usize);
        assert!(si < bi, "source before bus");
        assert_eq!(plan.tracks[si].sends.len(), 1);
        assert_eq!(plan.tracks[si].sends[0].dest as usize, bi);
        assert!((plan.tracks[si].sends[0].gain - 0.501).abs() < 0.01);
        // A send back from the bus to the source is a loop.
        let b = p.mixer.tracks.iter().position(|t| t.id == bus_id).unwrap();
        p.mixer.tracks[b].sends.push(peel_document::MixerSend {
            target: src,
            gain_db: 0.0,
        });
        assert!(peel_document::validate(&p).iter().any(|i| i.is_error()));
    }

    fn track(name: &str) -> MixerTrack {
        MixerTrack {
            id: Id::new(),
            name: name.into(),
            color: PALETTE[3],
            gain_db: 0.0,
            pan: 0.0,
            muted: false,
            solo: false,
            inserts: vec![],
            output: None,
            sends: vec![],
        }
    }

    /// Muted flags by track name under the project's solo settings.
    fn muted(p: &Project) -> HashMap<String, bool> {
        let (slots, meters) = ctx_for(p);
        let (plan, index) = compile(
            p,
            &CompileContext {
                mode: PlayMode::Song,
                slots: &slots,
                meters: &meters,
                samples: &HashMap::new(),
            },
        );
        p.mixer
            .tracks
            .iter()
            .map(|t| {
                (
                    t.name.clone(),
                    plan.tracks[index.tracks[&t.id] as usize].muted,
                )
            })
            .collect()
    }

    #[test]
    fn solo_keeps_the_path_to_the_master_and_the_returns() {
        // A and B both send to Verb; A plays through Bus; C is unrelated.
        let (mut p, _, _) = project();
        let (bus, verb, b, c) = (track("Bus"), track("Verb"), track("B"), track("C"));
        let a = p.mixer.tracks[1].id;
        p.mixer.tracks[1].output = Some(bus.id);
        p.mixer.tracks[1].sends.push(peel_document::MixerSend {
            target: verb.id,
            gain_db: -6.0,
        });
        let mut b = b;
        b.sends.push(peel_document::MixerSend {
            target: verb.id,
            gain_db: -6.0,
        });
        p.mixer.tracks.extend([bus, verb, b, c]);
        assert!(peel_document::validate(&p).iter().all(|i| !i.is_error()));
        p.mixer.track_mut(a).unwrap().solo = true;
        let m = muted(&p);
        assert!(!m["A"], "soloed");
        assert!(!m["Bus"], "on the soloed track's way to the master");
        assert!(!m["Verb"], "the soloed track's return");
        assert!(
            m["B"],
            "another source on the shared return is muted, so it cannot leak"
        );
        assert!(m["C"]);
        // Soloing the return brings in its sources.
        p.mixer.track_mut(a).unwrap().solo = false;
        let verb_id = p.mixer.tracks.iter().find(|t| t.name == "Verb").unwrap().id;
        p.mixer.track_mut(verb_id).unwrap().solo = true;
        let m = muted(&p);
        assert!(!m["A"] && !m["B"] && !m["Verb"]);
        assert!(m["C"]);
    }
    fn expression_fixture() -> (Project, Id, Id) {
        use peel_document::{ExpressionPoint, NoteExpression};
        let (mut p, channel, pid) = project();
        let pt = p.pattern_mut(pid).unwrap();
        pt.notes = vec![peel_document::Note::new(
            channel,
            Time::ZERO,
            Time::beats(4),
            60,
            0.8,
        )];
        pt.expression = vec![NoteExpression {
            note: pt.notes[0].id,
            pitch: vec![
                ExpressionPoint {
                    offset: Time::ZERO,
                    value: 1.0,
                },
                ExpressionPoint {
                    offset: Time::beats(1),
                    value: 2.0,
                },
                ExpressionPoint {
                    offset: Time::new(3, 2),
                    value: 3.0,
                },
            ],
            pressure: vec![ExpressionPoint {
                offset: Time::new(1, 3),
                value: 0.7,
            }],
            timbre: vec![ExpressionPoint {
                offset: Time::ZERO,
                value: 0.5,
            }],
        }];
        (p, channel, pid)
    }

    #[test]
    fn expression_points_compile_as_held_full_snapshots_after_their_note_on() {
        let (p, _, pid) = expression_fixture();
        let (slots, meters) = ctx_for(&p);
        let (plan, _) = compile(
            &p,
            &CompileContext {
                mode: PlayMode::Pattern(pid),
                slots: &slots,
                meters: &meters,
                samples: &HashMap::new(),
            },
        );
        assert_eq!(plan.expression_notes.len(), 1);
        assert!(matches!(plan.sequence[0].kind, SeqKind::NoteOn { .. }));
        assert!(matches!(
            plan.sequence[1].kind,
            SeqKind::Expression {
                pitch: 1.0,
                pressure: 0.0,
                timbre: 0.5,
                ..
            }
        ));
        assert!(matches!(
            plan.sequence[2].kind,
            SeqKind::Expression {
                pitch: 1.0,
                pressure: 0.7,
                ..
            }
        ));
        assert_eq!(plan.sequence[2].beat, 1.0 / 3.0);
        assert_eq!(plan.expression_notes[0].points.len(), 4);
        assert!(plan.sequence.windows(2).all(|w| w[0].beat <= w[1].beat));
    }

    #[test]
    fn trimmed_clips_chase_the_expression_at_the_visible_note_start() {
        let (mut p, _, pid) = expression_fixture();
        let lane = p.arrangement().tracks[0].id;
        p.arrangement_mut().clips = vec![Clip {
            id: Id::new(),
            track: lane,
            start: Time::beats(4),
            length: Time::beats(2),
            offset: Time::beats(1),
            muted: false,
            content: ClipContent::Pattern { pattern: pid },
        }];
        let (slots, meters) = ctx_for(&p);
        let (plan, _) = compile(
            &p,
            &CompileContext {
                mode: PlayMode::Song,
                slots: &slots,
                meters: &meters,
                samples: &HashMap::new(),
            },
        );
        let states: Vec<_> = plan
            .sequence
            .iter()
            .filter_map(|e| {
                if let SeqKind::Expression {
                    pitch, pressure, ..
                } = e.kind
                {
                    Some((e.beat, pitch, pressure))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(states, vec![(4.0, 2.0, 0.7), (4.5, 3.0, 0.7)]);
        assert_eq!(plan.expression_notes[0].start_beat, 4.0);
        assert_eq!(plan.expression_notes[0].end_beat, 6.0);
    }

    #[test]
    fn overlapping_clips_and_same_pitch_notes_have_distinct_expression_voices() {
        let (mut p, channel, pid) = expression_fixture();
        let second = peel_document::Note::new(channel, Time::ZERO, Time::beats(4), 60, 0.8);
        let pt = p.pattern_mut(pid).unwrap();
        pt.expression.push(peel_document::NoteExpression {
            note: second.id,
            ..Default::default()
        });
        pt.notes.push(second);
        let lane = p.arrangement().tracks[0].id;
        let clip_ids = [Id::from_u128(1), Id::from_u128(1 << 64)];
        assert_eq!(clip_ids[0].short(), clip_ids[1].short());
        p.arrangement_mut().clips = clip_ids
            .into_iter()
            .map(|id| Clip {
                id,
                track: lane,
                start: Time::ZERO,
                length: Time::beats(4),
                offset: Time::ZERO,
                muted: false,
                content: ClipContent::Pattern { pattern: pid },
            })
            .collect();
        let (slots, meters) = ctx_for(&p);
        let (plan, _) = compile(
            &p,
            &CompileContext {
                mode: PlayMode::Song,
                slots: &slots,
                meters: &meters,
                samples: &HashMap::new(),
            },
        );
        assert_eq!(plan.expression_notes.len(), 4);
        let voices: std::collections::HashSet<_> =
            plan.expression_notes.iter().map(|n| n.voice).collect();
        assert_eq!(voices.len(), 4);
        let identities: std::collections::BTreeSet<_> =
            plan.note_identities.iter().map(|n| n.identity).collect();
        assert_eq!(
            identities.len(),
            4,
            "full clip UUIDs keep even colliding short hashes independent"
        );
        assert_eq!(plan.note_identity_index.len(), 4);
        assert!(plan.note_identity_index.windows(2).all(|w| w[0].0 < w[1].0));
        assert!(
            plan.expression_notes
                .windows(2)
                .all(|w| w[0].voice < w[1].voice)
        );
        assert!(voices.iter().all(|&v| v < peel_engine::LIVE_VOICE_BASE));
    }
}
