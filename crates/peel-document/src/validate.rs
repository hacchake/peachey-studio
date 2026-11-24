//! Reference and range validation. Untrusted projects are validated before
//! they reach the engine; transactions are validated in debug builds.

use std::collections::HashSet;

use crate::{ClipContent, FORMAT, Id, Project};

#[derive(Clone, Debug, PartialEq)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Issue {
    pub severity: Severity,
    pub message: String,
}

impl Issue {
    fn error(message: impl Into<String>) -> Issue {
        Issue {
            severity: Severity::Error,
            message: message.into(),
        }
    }

    fn warning(message: impl Into<String>) -> Issue {
        Issue {
            severity: Severity::Warning,
            message: message.into(),
        }
    }

    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

/// Checks identity uniqueness, references and value ranges.
pub fn validate(p: &Project) -> Vec<Issue> {
    let mut issues = Vec::new();
    if p.format != FORMAT {
        issues.push(Issue::error(format!("unexpected format {:?}", p.format)));
    }
    let mut ids: HashSet<Id> = HashSet::new();
    let mut unique = |id: Id, what: &str, issues: &mut Vec<Issue>| {
        if id.is_nil() || !ids.insert(id) {
            issues.push(Issue::error(format!(
                "duplicate or nil id for {what}: {id}"
            )));
        }
    };

    if p.mixer.tracks.is_empty() {
        issues.push(Issue::error("mixer has no master track"));
    }
    let track_ids: HashSet<Id> = p.mixer.tracks.iter().map(|t| t.id).collect();
    for t in &p.mixer.tracks {
        unique(t.id, "mixer track", &mut issues);
        for d in &t.inserts {
            unique(d.id, "insert", &mut issues);
            if let Some(src) = d.sidechain
                && (!track_ids.contains(&src) || src == t.id)
            {
                issues.push(Issue::error(format!(
                    "an insert on mixer track {} has an invalid sidechain",
                    t.name
                )));
            }
        }
        for send in &t.sends {
            if !track_ids.contains(&send.target) || send.target == t.id || !send.gain_db.is_finite()
            {
                issues.push(Issue::error(format!(
                    "mixer track {} has an invalid send",
                    t.name
                )));
            }
        }
        if let Some(out) = t.output {
            if !track_ids.contains(&out) {
                issues.push(Issue::error(format!(
                    "mixer track {} routes to a missing track",
                    t.name
                )));
            }
            if out == t.id {
                issues.push(Issue::error(format!(
                    "mixer track {} routes to itself",
                    t.name
                )));
            }
        }
    }
    if let Some(cycle) = routing_cycle(p) {
        issues.push(Issue::error(format!("mixer routing cycle through {cycle}")));
    }

    let channel_ids: HashSet<Id> = p.channels.iter().map(|c| c.id).collect();
    for c in &p.channels {
        unique(c.id, "channel", &mut issues);
        unique(c.device.id, "channel device", &mut issues);
        if !track_ids.contains(&c.track) {
            issues.push(Issue::error(format!(
                "channel {} routes to a missing mixer track",
                c.name
            )));
        }
        if let Some(asset) = c.device.sample
            && p.asset(asset).is_none()
        {
            issues.push(Issue::warning(format!(
                "channel {} references a missing sample",
                c.name
            )));
        }
    }

    let pattern_ids: HashSet<Id> = p.patterns.iter().map(|pt| pt.id).collect();
    for pt in &p.patterns {
        unique(pt.id, "pattern", &mut issues);
        if !pt.length.is_positive() {
            issues.push(Issue::error(format!(
                "pattern {} has a non-positive length",
                pt.name
            )));
        }
        for n in &pt.notes {
            unique(n.id, "note", &mut issues);
            if !channel_ids.contains(&n.channel) {
                issues.push(Issue::error(format!(
                    "pattern {} has a note for a missing channel",
                    pt.name
                )));
            }
            if !n.length.is_positive() {
                issues.push(Issue::error(format!(
                    "pattern {} has a note with non-positive length",
                    pt.name
                )));
            }
            if n.key > 127 || !(0.0..=1.0).contains(&n.velocity) {
                issues.push(Issue::error(format!(
                    "pattern {} has a note out of range",
                    pt.name
                )));
            }
        }
        let notes: std::collections::HashMap<_, _> = pt.notes.iter().map(|n| (n.id, n)).collect();
        let mut expressed = HashSet::new();
        let mut pattern_points = 0usize;
        for expression in &pt.expression {
            let Some(note) = notes.get(&expression.note) else {
                issues.push(Issue::error(format!(
                    "pattern {} has expression for a missing note",
                    pt.name
                )));
                continue;
            };
            if !expressed.insert(expression.note) {
                issues.push(Issue::error(format!(
                    "pattern {} has duplicate note expression",
                    pt.name
                )));
            }
            let count = expression
                .pitch
                .len()
                .saturating_add(expression.pressure.len())
                .saturating_add(expression.timbre.len());
            pattern_points = pattern_points.saturating_add(count);
            if count > crate::MAX_EXPRESSION_POINTS {
                issues.push(Issue::error(format!(
                    "pattern {} has too many expression points for one note",
                    pt.name
                )));
            }
            for (points, min, max) in [
                (&expression.pitch, -120.0, 120.0),
                (&expression.pressure, 0.0, 1.0),
                (&expression.timbre, 0.0, 1.0),
            ] {
                if points.iter().any(|p| {
                    p.offset.is_negative()
                        || p.offset > note.length
                        || !p.value.is_finite()
                        || !(min..=max).contains(&p.value)
                }) {
                    issues.push(Issue::error(format!(
                        "pattern {} has an expression point out of range",
                        pt.name
                    )));
                }
                if points.windows(2).any(|w| w[0].offset >= w[1].offset) {
                    issues.push(Issue::error(format!(
                        "pattern {} has unsorted or duplicate expression offsets",
                        pt.name
                    )));
                }
            }
        }
        if pattern_points > 1_000_000 {
            issues.push(Issue::error(format!(
                "pattern {} has too many expression points",
                pt.name
            )));
        }
    }

    if p.arrangements.is_empty() {
        issues.push(Issue::error("project has no arrangement"));
    }
    for a in &p.arrangements {
        unique(a.id, "arrangement", &mut issues);
        let lanes: HashSet<Id> = a.tracks.iter().map(|t| t.id).collect();
        for t in &a.tracks {
            unique(t.id, "playlist track", &mut issues);
        }
        for c in &a.clips {
            unique(c.id, "clip", &mut issues);
            if !lanes.contains(&c.track) {
                issues.push(Issue::error("clip on a missing playlist track"));
            }
            if !c.length.is_positive() {
                issues.push(Issue::error("clip with non-positive length"));
            }
            match &c.content {
                ClipContent::Pattern { pattern } => {
                    if !pattern_ids.contains(pattern) {
                        issues.push(Issue::error("clip references a missing pattern"));
                    }
                }
                ClipContent::Audio {
                    asset,
                    track,
                    gain_db,
                    fade_in,
                    fade_out,
                    ..
                } => {
                    if p.asset(*asset).is_none() {
                        issues.push(Issue::warning("audio clip references a missing asset"));
                    }
                    if !track_ids.contains(track) {
                        issues.push(Issue::error("audio clip routes to a missing mixer track"));
                    }
                    if fade_in.is_negative() || fade_out.is_negative() {
                        issues.push(Issue::error("audio clip with a negative fade"));
                    }
                    if !gain_db.is_finite() {
                        issues.push(Issue::error("audio clip with a non-finite gain"));
                    }
                }
                ClipContent::Automation { target, points } => {
                    let target_ok = match target {
                        crate::AutomationTarget::Param { device, .. } => {
                            p.channels.iter().any(|c| c.device.id == *device)
                                || p.mixer
                                    .tracks
                                    .iter()
                                    .any(|t| t.inserts.iter().any(|d| d.id == *device))
                        }
                        crate::AutomationTarget::TrackGain { track }
                        | crate::AutomationTarget::TrackPan { track } => track_ids.contains(track),
                        crate::AutomationTarget::MidiCc { device, cc } => {
                            if *cc > 127 {
                                issues.push(Issue::error("MIDI controller number out of range"));
                            }
                            p.channels.iter().any(|c| c.device.id == *device)
                        }
                    };
                    if !target_ok {
                        // Kept with its envelope so it can be rebound.
                        issues.push(Issue::warning(
                            "automation clip targets a missing parameter",
                        ));
                    }
                    if points
                        .iter()
                        .any(|pt| !(0.0..=1.0).contains(&pt.value) || !pt.value.is_finite())
                    {
                        issues.push(Issue::error("automation point out of range"));
                    }
                    if points.windows(2).any(|w| w[1].position < w[0].position) {
                        issues.push(Issue::error("automation points out of order"));
                    }
                }
            }
        }
    }
    if !p.arrangements.iter().any(|a| a.id == p.active_arrangement) {
        issues.push(Issue::warning("active arrangement is missing"));
    }
    for a in &p.assets {
        unique(a.id, "asset", &mut issues);
    }
    issues
}

/// Name of a track on a routing cycle, if any.
fn routing_cycle(p: &Project) -> Option<String> {
    p.mixer.tracks.first()?;
    // Depth-first search over outputs, sends and sidechain keys; a back
    // edge is a cycle.
    fn visit(p: &Project, id: Id, state: &mut std::collections::HashMap<Id, u8>) -> Option<Id> {
        match state.get(&id) {
            Some(1) => return Some(id),
            Some(2) => return None,
            _ => {}
        }
        state.insert(id, 1);
        if p.mixer.track(id).is_some() {
            for n in p.mixer.feeds(id) {
                if p.mixer.track(n).is_some()
                    && let Some(c) = visit(p, n, state)
                {
                    return Some(c);
                }
            }
        }
        state.insert(id, 2);
        None
    }
    let mut state = std::collections::HashMap::new();
    for t in &p.mixer.tracks {
        if let Some(c) = visit(p, t.id, &mut state) {
            return Some(
                p.mixer
                    .track(c)
                    .map_or_else(|| t.name.clone(), |t| t.name.clone()),
            );
        }
    }
    None
}
