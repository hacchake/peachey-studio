//! Behavioural tests that every instrument must pass.

mod common;

use common::*;
use peel_dsp::{DeviceKind, EventKind};

/// Types under test: every required type that the crate provides. The
/// `all_required_types_exist` test fails if one is missing.
fn types() -> Vec<&'static str> {
    REQUIRED
        .iter()
        .copied()
        .filter(|t| peel_instruments::create(t).is_some())
        .collect()
}

/// A short phrase: overlapping notes, release, and parameter changes.
fn phrase() -> Vec<Timed> {
    vec![
        note_on(100, 60, 0.8, 1),
        note_on(3_000, 64, 0.6, 2),
        note_on(3_017, 67, 0.9, 3),
        note_off(15_000, 60, 1),
        note_on(17_777, 72, 1.0, 4),
        note_off(20_001, 64, 2),
        note_off(24_000, 67, 3),
        note_off(30_011, 72, 4),
    ]
}

/// The phrase plus normalized-parameter events scattered through it.
fn phrase_with_params(type_id: &str) -> Vec<Timed> {
    let mut ev = phrase();
    let desc = peel_instruments::descriptors()
        .iter()
        .find(|d| d.type_id == type_id)
        .unwrap();
    let mut rng = Lcg(99);
    for (i, p) in desc.params.iter().enumerate() {
        let at = 500 + (i * 1_913) % 28_000;
        ev.push((
            at,
            EventKind::Param {
                id: p.id,
                value: p.from_normalized(0.15 + 0.7 * rng.next_f32()),
            },
        ));
    }
    ev.sort_by_key(|(t, _)| *t);
    ev
}

#[test]
fn all_required_types_exist() {
    for id in REQUIRED {
        assert!(peel_instruments::create(id).is_some(), "missing {id}");
    }
    let ids: Vec<_> = peel_instruments::descriptors()
        .iter()
        .map(|d| d.type_id)
        .collect();
    for id in REQUIRED {
        assert!(ids.contains(&id), "descriptor missing for {id}");
    }
    assert!(peel_instruments::create("peel.nope").is_none());
}

#[test]
fn descriptors_are_well_formed() {
    for d in peel_instruments::descriptors() {
        assert_eq!(d.kind, DeviceKind::Instrument);
        assert!(!d.name.is_empty() && !d.description.is_empty() && !d.category.is_empty());
        assert!(
            (6..=48).contains(&d.params.len()),
            "{}: {} params",
            d.type_id,
            d.params.len()
        );
        let mut groups: Vec<&str> = d.params.iter().map(|p| p.group).collect();
        groups.sort();
        groups.dedup();
        assert!(
            (2..=5).contains(&groups.len()),
            "{}: {} groups",
            d.type_id,
            groups.len()
        );
        for (i, p) in d.params.iter().enumerate() {
            assert!(p.id != 0, "{} param ids start at 1", d.type_id);
            assert!(!p.key.is_empty() && !p.name.is_empty());
            assert!(p.min < p.max, "{}:{} empty range", d.type_id, p.key);
            assert!(
                p.default >= p.min && p.default <= p.max,
                "{}:{} default out of range",
                d.type_id,
                p.key
            );
            if p.is_choice() {
                assert_eq!(p.default.fract(), 0.0, "{}:{}", d.type_id, p.key);
            }
            if matches!(p.taper, peel_dsp::Taper::Log) {
                assert!(
                    p.min > 0.0,
                    "{}:{} log taper needs min > 0",
                    d.type_id,
                    p.key
                );
            }
            for q in &d.params[i + 1..] {
                assert_ne!(p.id, q.id, "{} duplicate id", d.type_id);
                assert_ne!(p.key, q.key, "{} duplicate key", d.type_id);
            }
        }
    }
}

#[test]
fn presets_are_valid_and_first_matches_defaults() {
    for d in peel_instruments::descriptors() {
        let presets = peel_instruments::presets(d.type_id);
        assert!(
            (3..=32).contains(&presets.len()),
            "{}: {} presets",
            d.type_id,
            presets.len()
        );
        let mut names: Vec<_> = presets.iter().map(|p| p.name).collect();
        names.sort();
        names.dedup();
        assert_eq!(
            names.len(),
            presets.len(),
            "{} preset names unique",
            d.type_id
        );
        for preset in presets {
            for (key, value) in preset.params {
                let p = d.param_by_key(key).unwrap_or_else(|| {
                    panic!("{} preset {}: unknown key {key}", d.type_id, preset.name)
                });
                assert!(
                    *value >= p.min && *value <= p.max,
                    "{} preset {}: {key} = {value} outside {}..{}",
                    d.type_id,
                    preset.name,
                    p.min,
                    p.max
                );
            }
        }
        for (key, value) in presets[0].params {
            let p = d.param_by_key(key).unwrap();
            assert!(
                (p.default - value).abs() <= 1e-4 * p.default.abs().max(1.0),
                "{}: first preset {key} = {value} but default is {}",
                d.type_id,
                p.default
            );
        }
    }
}

#[test]
fn every_preset_plays_finite_audible_audio() {
    for d in peel_instruments::descriptors() {
        for preset in peel_instruments::presets(d.type_id) {
            let mut dev = fresh(d.type_id);
            for (key, value) in preset.params {
                dev.set_param(d.param_by_key(key).unwrap().id, *value);
            }
            // The recorded kit's MIDI mapping leaves key 60 silent, as
            // General MIDI drum maps do; audition it on the snare.
            let key = if d.type_id == "peel.drum.acoustic" && preset.name == "Full Kit" {
                38
            } else {
                60
            };
            let (l, r) = render(
                dev.as_mut(),
                &[note_on(0, key, 0.9, 1), note_off(24_000, key, 1)],
                48_000,
                256,
            );
            let pk = peak(&l).max(peak(&r));
            assert!(
                all_finite(&l) && all_finite(&r),
                "{} {}",
                d.type_id,
                preset.name
            );
            assert!(
                (0.02..2.0).contains(&pk),
                "{} preset {}: peak {pk}",
                d.type_id,
                preset.name
            );
        }
    }
}

#[test]
fn note_on_is_audible_finite_and_sane() {
    for id in types() {
        let mut dev = fresh(id);
        assert!(dev.is_silent(), "{id} should start silent");
        let (l, r) = render(
            dev.as_mut(),
            &[note_on(100, 60, 0.8, 1), note_off(24_000, 60, 1)],
            48_000,
            256,
        );
        assert!(
            all_finite(&l) && all_finite(&r),
            "{id} produced non-finite output"
        );
        let pk = peak(&l).max(peak(&r));
        assert!(pk > 0.01, "{id}: peak {pk} too quiet");
        assert!(pk < 2.0, "{id}: peak {pk} too loud");
    }
}

#[test]
fn becomes_silent_after_note_off() {
    for id in types() {
        let mut dev = fresh(id);
        let (l, r) = render(
            dev.as_mut(),
            &[note_on(0, 60, 0.9, 1), note_off(12_000, 60, 1)],
            48_000 * 12,
            512,
        );
        assert!(dev.is_silent(), "{id}: is_silent() false after the tail");
        let n = l.len();
        assert!(
            peak(&l[n - 4_000..]) < 1e-4 && peak(&r[n - 4_000..]) < 1e-4,
            "{id}: not silent at the end"
        );
        // Once silent the instrument must keep producing exact silence.
        let (l2, r2) = render(dev.as_mut(), &[], 4_096, 256);
        assert_eq!(peak(&l2).max(peak(&r2)), 0.0, "{id}: noise while idle");
    }
}

#[test]
fn no_output_before_note_on() {
    for id in types() {
        let mut dev = fresh(id);
        let (l, r) = render(dev.as_mut(), &[note_on(300, 60, 1.0, 1)], 4_000, 4_000);
        assert_eq!(peak(&l[..300]), 0.0, "{id}: output before note-on (left)");
        assert_eq!(peak(&r[..300]), 0.0, "{id}: output before note-on (right)");
        assert!(
            peak(&l[300..]).max(peak(&r[300..])) > 0.0,
            "{id}: no output after note-on"
        );
    }
}

#[test]
fn extreme_parameters_stay_finite() {
    let chord = [
        note_on(0, 36, 1.0, 1),
        note_on(10, 60, 1.0, 2),
        note_on(20, 84, 1.0, 3),
        note_on(30, 127, 1.0, 4),
        note_on(40, 0, 1.0, 5),
        note_off(9_000, 60, 2),
        note_on(10_000, 60, 0.0, 6),
    ];
    for d in peel_instruments::descriptors() {
        let check = |label: &str, setup: &dyn Fn(&mut dyn peel_dsp::Device)| {
            let mut dev = fresh(d.type_id);
            setup(dev.as_mut());
            let (l, r) = render(dev.as_mut(), &chord, 24_000, 256);
            assert!(
                all_finite(&l) && all_finite(&r),
                "{}: non-finite output with {label}",
                d.type_id
            );
            let pk = peak(&l).max(peak(&r));
            assert!(pk < 6.0, "{}: peak {pk} with {label}", d.type_id);
        };
        for p in d.params {
            for (tag, v) in [("min", p.min), ("max", p.max)] {
                check(&format!("{} = {tag}", p.key), &|dev| dev.set_param(p.id, v));
            }
        }
        check("all at min", &|dev| {
            for p in d.params {
                dev.set_param(p.id, p.min);
            }
        });
        check("all at max", &|dev| {
            for p in d.params {
                dev.set_param(p.id, p.max);
            }
        });
        let mut rng = Lcg(12345);
        for round in 0..12 {
            let values: Vec<f32> = d
                .params
                .iter()
                .map(|p| {
                    // Favor the extremes.
                    let r = rng.next_f32();
                    let n = if r < 0.3 {
                        0.0
                    } else if r > 0.7 {
                        1.0
                    } else {
                        rng.next_f32()
                    };
                    p.from_normalized(n)
                })
                .collect();
            check(&format!("random round {round}"), &|dev| {
                for (p, v) in d.params.iter().zip(&values) {
                    dev.set_param(p.id, *v);
                }
            });
        }
        // Garbage values are clamped, never propagated.
        check("NaN and infinity", &|dev| {
            for p in d.params {
                dev.set_param(p.id, f32::NAN);
                dev.set_param(p.id, f32::INFINITY);
                dev.set_param(p.id, f32::NEG_INFINITY);
            }
        });
    }
}

#[test]
fn is_deterministic() {
    for id in types() {
        let events = phrase_with_params(id);
        let mut a = fresh(id);
        let mut b = fresh(id);
        let (al, ar) = render(a.as_mut(), &events, 40_000, 256);
        let (bl, br) = render(b.as_mut(), &events, 40_000, 256);
        assert_eq!(al, bl, "{id}: left differs between instances");
        assert_eq!(ar, br, "{id}: right differs between instances");
    }
}

#[test]
fn output_is_independent_of_block_size() {
    for id in types() {
        let events = phrase_with_params(id);
        let mut reference = fresh(id);
        let (rl, rr) = render(reference.as_mut(), &events, 44_000, 256);
        assert!(peak(&rl).max(peak(&rr)) > 0.01, "{id}: silent reference");
        for block in [32, 1, 480, 97] {
            let mut dev = fresh(id);
            let (l, r) = render(dev.as_mut(), &events, 44_000, block);
            let diff = max_diff(&l, &rl).max(max_diff(&r, &rr));
            assert!(
                diff < 1e-4,
                "{id}: block {block} differs from block 256 by {diff}"
            );
        }
    }
}

#[test]
fn all_sound_off_silences_immediately() {
    for id in types() {
        let mut dev = fresh(id);
        let events = [
            note_on(0, 60, 1.0, 1),
            note_on(0, 64, 1.0, 2),
            (8_000, EventKind::AllSoundOff),
        ];
        let (l, r) = render(dev.as_mut(), &events, 12_000, 256);
        assert!(
            peak(&l[8_000..]) + peak(&r[8_000..]) == 0.0,
            "{id}: output after AllSoundOff"
        );
        assert!(dev.is_silent(), "{id}: not silent after AllSoundOff");
    }
}

#[test]
fn all_notes_off_releases_held_notes() {
    for id in types() {
        let mut dev = fresh(id);
        let events = [
            note_on(0, 60, 1.0, 1),
            note_on(0, 67, 1.0, 2),
            (4_000, EventKind::AllNotesOff),
        ];
        let _ = render(dev.as_mut(), &events, 48_000 * 12, 512);
        assert!(dev.is_silent(), "{id}: AllNotesOff did not end the sound");
    }
}

#[test]
fn survives_a_flood_of_notes() {
    for id in types() {
        let mut dev = fresh(id);
        let mut events = Vec::new();
        for i in 0..80u32 {
            let at = (i as usize) * 301;
            events.push(note_on(at, 36 + (i % 48) as u8, 0.7, i));
            if i % 3 == 0 {
                events.push(note_off(at + 2_000, 36 + (i % 48) as u8, i));
            }
        }
        events.sort_by_key(|(t, _)| *t);
        let (l, r) = render(dev.as_mut(), &events, 48_000, 128);
        assert!(all_finite(&l) && all_finite(&r), "{id}");
        // 16 overlapping loud voices may legitimately add up; the output rail
        // (+12 dBFS) must still hold.
        assert!(
            peak(&l).max(peak(&r)) <= 4.0,
            "{id}: flood peak beyond the rail"
        );
        let _ = render(
            dev.as_mut(),
            &[(0, EventKind::AllNotesOff)],
            48_000 * 15,
            1024,
        );
        assert!(dev.is_silent(), "{id}: still sounding after flood tail");
    }
}

#[test]
fn reset_clears_state_and_prepare_is_repeatable() {
    for id in types() {
        let mut dev = fresh(id);
        let events = phrase();
        let (a, _) = render(dev.as_mut(), &events, 20_000, 256);
        dev.reset();
        assert!(dev.is_silent(), "{id}: reset left voices sounding");
        let (b, _) = render(dev.as_mut(), &[], 1_000, 256);
        assert_eq!(peak(&b), 0.0, "{id}: output after reset");
        // Preparing again at another rate and block size keeps it usable.
        dev.prepare(44_100.0, 64);
        let (c, _) = render(dev.as_mut(), &events[..2], 8_000, 64);
        assert!(all_finite(&a) && all_finite(&c));
        assert!(peak(&c) > 0.0, "{id}: silent after re-prepare");
    }
}

#[test]
fn params_round_trip_and_clamp() {
    for d in peel_instruments::descriptors() {
        let mut dev = fresh(d.type_id);
        for p in d.params {
            assert_eq!(dev.param(p.id), p.default, "{}:{}", d.type_id, p.key);
            dev.set_param(p.id, p.max + 1_000.0);
            assert_eq!(
                dev.param(p.id),
                p.max,
                "{}:{} clamps high",
                d.type_id,
                p.key
            );
            dev.set_param(p.id, p.min - 1_000.0);
            assert_eq!(dev.param(p.id), p.min, "{}:{} clamps low", d.type_id, p.key);
            dev.set_param(p.id, f32::NAN);
            assert!(dev.param(p.id).is_finite());
        }
        dev.set_param(0xDEAD_BEEF, 1.0); // unknown ids are ignored
    }
}

#[test]
fn velocity_changes_loudness() {
    for id in types() {
        let level = |velocity: f32| {
            let mut dev = fresh(id);
            let (l, r) = render(
                dev.as_mut(),
                &[note_on(0, 60, velocity, 1), note_off(12_000, 60, 1)],
                24_000,
                256,
            );
            peak(&l).max(peak(&r))
        };
        let soft = level(0.25);
        let hard = level(1.0);
        assert!(hard > soft * 1.2, "{id}: soft {soft}, hard {hard}");
    }
}

#[test]
fn pitch_follows_the_key() {
    // Pitched instruments: an octave up should roughly double the frequency.
    for id in [
        "peel.synth.poly",
        "peel.synth.bass",
        "peel.keys.tine",
        "peel.keys.grand",
        "peel.guitar.electric",
        "peel.sampler",
        "peel.wind.sax",
    ] {
        if peel_instruments::create(id).is_none() {
            continue;
        }
        let freq = |key: u8| {
            let mut dev = fresh(id);
            let (l, _) = render(dev.as_mut(), &[note_on(0, key, 0.8, 1)], 48_000 / 2, 256);
            detect_pitch(&l[4_800..], SR)
        };
        let low = freq(57);
        let high = freq(69);
        assert!(
            low > 0.0 && high > 0.0,
            "{id}: no pitch found ({low}, {high})"
        );
        // Zero crossings of complex timbres are noisy; allow generous error
        // but still catch an instrument that ignores the key.
        let ratio = high / low;
        assert!(
            (1.6..2.5).contains(&ratio),
            "{id}: octave ratio {ratio} (low {low} Hz, high {high} Hz)"
        );
    }
}

#[test]
fn drums_transpose_with_key() {
    for id in ["peel.drum.kick", "peel.drum.tom", "peel.drum.snare"] {
        if peel_instruments::create(id).is_none() {
            continue;
        }
        let centroid = |key: u8| {
            let mut dev = fresh(id);
            let (l, _) = render(dev.as_mut(), &[note_on(0, key, 1.0, 1)], 12_000, 256);
            // Mean absolute first difference relative to level: a proxy for
            // spectral centroid.
            let diff: f32 = l.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
            let sum: f32 = l.iter().map(|x| x.abs()).sum();
            diff / sum.max(1e-9)
        };
        assert!(
            centroid(72) > centroid(48) * 1.2,
            "{id}: higher key should sound higher"
        );
    }
}

#[test]
fn param_events_take_effect_at_their_sample_and_not_before() {
    for id in types() {
        let desc = peel_instruments::descriptors()
            .iter()
            .find(|d| d.type_id == id)
            .unwrap();
        let notes = [note_on(100, 60, 0.9, 1), note_on(120, 67, 0.9, 2)];
        let mut plain = fresh(id);
        let (a, _) = render(plain.as_mut(), &notes, 8_000, 256);

        let mut events = notes.to_vec();
        for p in desc.params {
            events.push((
                3_000,
                EventKind::Param {
                    id: p.id,
                    value: p.max,
                },
            ));
        }
        events.sort_by_key(|(t, _)| *t);
        let mut changed = fresh(id);
        let (b, _) = render(changed.as_mut(), &events, 8_000, 256);
        assert_eq!(
            &a[..3_000],
            &b[..3_000],
            "{id}: parameter event leaked into the past"
        );
        assert!(
            max_diff(&a[3_000..], &b[3_000..]) > 1e-4,
            "{id}: parameter event had no audible effect"
        );
    }
}

fn pedal(at: usize, down: bool) -> Timed {
    (
        at,
        EventKind::Controller {
            cc: 64,
            value: if down { 1.0 } else { 0.0 },
        },
    )
}

/// Peak level of `buf` over `[from, to)`.
fn level(buf: &[f32], from: usize, to: usize) -> f32 {
    peak(&buf[from..to])
}

#[test]
fn the_sustain_pedal_holds_releases_until_it_lifts() {
    // A sustaining keyboard: without the pedal the note dies soon after release.
    let t = "peel.synth.poly";
    let sr = SR as usize;
    let dry = render(
        fresh(t).as_mut(),
        &[note_on(0, 60, 0.9, 1), note_off(sr / 4, 60, 1)],
        sr * 2,
        256,
    )
    .0;
    let held = render(
        fresh(t).as_mut(),
        &[
            pedal(10, true),
            note_on(100, 60, 0.9, 1),
            note_off(sr / 4, 60, 1),
            pedal(sr, false),
        ],
        sr * 2,
        256,
    )
    .0;
    let window = (sr * 3 / 4, sr * 9 / 10);
    assert!(
        level(&dry, window.0, window.1) < 0.01,
        "released note has faded: {}",
        level(&dry, window.0, window.1)
    );
    assert!(
        level(&held, window.0, window.1) > 0.05,
        "the pedal keeps it sounding: {}",
        level(&held, window.0, window.1)
    );
    assert!(
        level(&held, sr * 7 / 4, sr * 2) < 0.01,
        "lifting the pedal releases it: {}",
        level(&held, sr * 7 / 4, sr * 2)
    );
}

#[test]
fn other_controllers_and_panic_leave_no_note_held() {
    let t = "peel.synth.poly";
    let sr = SR as usize;
    let out = render(
        fresh(t).as_mut(),
        &[
            (5, EventKind::Controller { cc: 1, value: 0.7 }),
            pedal(10, true),
            note_on(100, 60, 0.9, 1),
            note_off(sr / 4, 60, 1),
            (sr / 2, EventKind::AllNotesOff),
        ],
        sr * 2,
        256,
    )
    .0;
    assert!(
        level(&out, sr * 7 / 4, sr * 2) < 0.01,
        "all notes off clears the pedal's held releases"
    );
    assert!(all_finite(&out));
}
