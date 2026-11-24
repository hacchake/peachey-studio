//! `ClapDevice` against the real fixture plugins.

mod fixture;

use fixture::*;
use peel_dsp::{Device, DeviceEvent, DeviceKind, EventKind, ProcessContext};
use peel_host_clap::{ClapDevice, ClapError};

const GAIN_ID: u32 = 7;
const INVERT_ID: u32 = 9;
const LEVEL_ID: u32 = 100;

fn ctx() -> ProcessContext {
    ProcessContext::new(SR)
}

fn load(id: &str) -> ClapDevice {
    ClapDevice::load(&info(id), SR, BLOCK).unwrap_or_else(|e| panic!("load {id}: {e}"))
}

fn at(offset: u32, kind: EventKind) -> DeviceEvent {
    DeviceEvent { offset, kind }
}

fn note_on(offset: u32, key: u8, voice: u32) -> DeviceEvent {
    at(
        offset,
        EventKind::NoteOn {
            key,
            velocity: 1.0,
            voice,
        },
    )
}

fn note_off(offset: u32, key: u8, voice: u32) -> DeviceEvent {
    at(
        offset,
        EventKind::NoteOff {
            key,
            velocity: 0.0,
            voice,
        },
    )
}

fn run(dev: &mut ClapDevice, events: &[DeviceEvent], frames: usize) -> (Vec<f32>, Vec<f32>) {
    let (mut l, mut r) = (vec![0.0; frames], vec![0.0; frames]);
    dev.process(&ctx(), events, &mut l, &mut r);
    (l, r)
}

fn run_effect(dev: &mut ClapDevice, events: &[DeviceEvent], input: &[f32]) -> (Vec<f32>, Vec<f32>) {
    let (mut l, mut r) = (input.to_vec(), input.to_vec());
    dev.process(&ctx(), events, &mut l, &mut r);
    (l, r)
}

fn sine(key_hz: f64, frames: std::ops::Range<usize>, start: usize, amp: f64) -> Vec<f32> {
    frames
        .map(|i| {
            (amp * (std::f64::consts::TAU * key_hz * (i - start) as f64 / f64::from(SR)).sin())
                as f32
        })
        .collect()
}

// ---- descriptors -----------------------------------------------------------

#[test]
fn effect_descriptor_maps_clap_params() {
    let dev = load(GAIN);
    let d = dev.descriptor();
    assert_eq!(d.type_id, "clap:dev.peel.test.gain");
    assert_eq!(d.name, "Test Gain");
    assert_eq!(d.description, "Multiplies the signal");
    assert_eq!(d.category, "CLAP");
    assert_eq!(d.kind, DeviceKind::Effect);
    assert_eq!(d.params.len(), 2);

    let gain = &d.params[0];
    assert_eq!((gain.id, gain.key), (GAIN_ID, "7"));
    assert_eq!((gain.name, gain.group), ("Gain", "Main"));
    assert_eq!((gain.min, gain.max, gain.default), (0.0, 4.0, 1.0));
    assert!(gain.automatable && !gain.is_choice());

    // A stepped, enumerated parameter becomes a choice labelled by the
    // plugin's own text.
    let phase = &d.params[1];
    assert_eq!((phase.id, phase.key), (INVERT_ID, "9"));
    assert_eq!((phase.name, phase.group), ("Phase", "Main / Phase"));
    assert_eq!(phase.choices, ["Normal", "Inverted"]);
    assert_eq!((phase.min, phase.max, phase.default), (0.0, 1.0, 0.0));

    assert_eq!(d.param_by_key("7").map(|p| p.id), Some(GAIN_ID));
    assert_eq!(dev.param(GAIN_ID), 1.0, "starts at the plugin's own value");
}

#[test]
fn instrument_descriptor_hides_hidden_and_read_only_params() {
    let dev = load(SINE);
    let d = dev.descriptor();
    assert_eq!(d.type_id, "clap:dev.peel.test.sine");
    assert_eq!(d.kind, DeviceKind::Instrument);
    assert_eq!(
        d.params.len(),
        1,
        "Secret (hidden) and Voices (read only) are not exposed"
    );
    assert_eq!(d.params[0].id, LEVEL_ID);
    assert_eq!(d.params[0].group, "Osc");
    assert_eq!(dev.param(LEVEL_ID), 0.5);
    assert_eq!(dev.param(101), 0.0, "unknown ids read as zero");
}

#[test]
fn repeated_loads_share_the_leaked_descriptor() {
    let a = load(GAIN);
    let b = ClapDevice::load(&info(GAIN), 44_100.0, 128).unwrap();
    assert!(std::ptr::eq(a.descriptor(), b.descriptor()));
    let c = load(SINE);
    assert!(!std::ptr::eq(a.descriptor(), c.descriptor()));
}

#[test]
fn only_instruments_and_effects_can_be_loaded() {
    let mut note_effect = info(GAIN);
    note_effect.role = peel_host_clap::ClapRole::NoteEffect;
    let err = ClapDevice::load(&note_effect, SR, BLOCK).expect_err("must fail");
    assert!(matches!(err, ClapError::Unsupported(_)), "{err}");

    let mut missing = info(GAIN);
    missing.id = "dev.peel.test.nope".into();
    let err = ClapDevice::load(&missing, SR, BLOCK).expect_err("must fail");
    assert!(matches!(err, ClapError::PluginNotFound { .. }), "{err}");

    let err = ClapDevice::load(&info(GAIN), 0.0, BLOCK).expect_err("must fail");
    assert!(matches!(err, ClapError::InvalidArgument(_)), "{err}");
}

// ---- effects ---------------------------------------------------------------

#[test]
fn gain_leaves_the_signal_alone_at_unity_and_scales_it_exactly() {
    let mut dev = load(GAIN);
    let input = ramp(BLOCK);

    let (l, r) = run_effect(&mut dev, &[], &input);
    assert_eq!(l, input);
    assert_eq!(r, input);

    // set_param is delivered at the start of the next block.
    dev.set_param(GAIN_ID, 0.5);
    assert_eq!(dev.param(GAIN_ID), 0.5);
    let (l, r) = run_effect(&mut dev, &[], &input);
    let half: Vec<f32> = input.iter().map(|x| x * 0.5).collect();
    assert_eq!(l, half);
    assert_eq!(r, half);

    // Out-of-range values clamp to the plugin's range.
    dev.set_param(GAIN_ID, 9.0);
    assert_eq!(dev.param(GAIN_ID), 4.0);
    dev.set_param(GAIN_ID, -3.0);
    assert_eq!(dev.param(GAIN_ID), 0.0);
    dev.set_param(GAIN_ID, f32::NAN);
    assert_eq!(dev.param(GAIN_ID), 0.0, "non-finite values are ignored");
    let (l, _) = run_effect(&mut dev, &[], &input);
    assert!(l.iter().all(|x| *x == 0.0));
    assert_eq!(dev.stats().process_errors, 0);
}

#[test]
fn the_last_set_param_before_a_block_wins() {
    let mut dev = load(GAIN);
    dev.set_param(GAIN_ID, 2.0);
    dev.set_param(GAIN_ID, 0.25);
    dev.set_param(INVERT_ID, 1.0);
    let input = ramp(64);
    let (l, _) = run_effect(&mut dev, &[], &input);
    let expect: Vec<f32> = input.iter().map(|x| -x * 0.25).collect();
    assert_eq!(l, expect);
}

#[test]
fn param_events_land_on_the_exact_frame() {
    let mut dev = load(GAIN);
    let input = vec![1.0f32; BLOCK];
    let events = [
        at(
            100,
            EventKind::Param {
                id: GAIN_ID,
                value: 0.0,
            },
        ),
        at(
            300,
            EventKind::Param {
                id: GAIN_ID,
                value: 2.0,
            },
        ),
        at(
            301,
            EventKind::Param {
                id: INVERT_ID,
                value: 1.0,
            },
        ),
    ];
    let (l, r) = run_effect(&mut dev, &events, &input);
    assert!(l[..100].iter().all(|x| *x == 1.0), "before the first event");
    assert_eq!(l[99], 1.0);
    assert_eq!(
        l[100], 0.0,
        "the event's own frame already has the new value"
    );
    assert!(l[100..300].iter().all(|x| *x == 0.0));
    assert_eq!(l[300], 2.0);
    assert_eq!(l[301], -2.0);
    assert!(l[301..].iter().all(|x| *x == -2.0));
    assert_eq!(l, r);
    assert_eq!(dev.param(GAIN_ID), 2.0);
    assert_eq!(dev.param(INVERT_ID), 1.0);

    // The values persist into the next block.
    let (l, _) = run_effect(&mut dev, &[], &input);
    assert!(l.iter().all(|x| *x == -2.0));
}

#[test]
fn stepped_params_round_and_unknown_ids_are_ignored() {
    let mut dev = load(GAIN);
    dev.set_param(INVERT_ID, 0.6);
    assert_eq!(dev.param(INVERT_ID), 1.0);
    dev.set_param(12345, 1.0);
    assert_eq!(dev.param(12345), 0.0);
    let events = [at(
        0,
        EventKind::Param {
            id: 12345,
            value: 7.0,
        },
    )];
    let (l, _) = run_effect(&mut dev, &events, &[0.5; 8]);
    assert_eq!(l, [-0.5; 8], "the unknown id changed nothing, invert is on");
}

#[test]
fn mono_effect_gets_a_mix_and_returns_to_both_sides() {
    let mut dev = load(MONO_GAIN);
    dev.set_param(GAIN_ID, 2.0);
    let mut l = vec![1.0f32; 32];
    let mut r = vec![0.0f32; 32];
    dev.process(&ctx(), &[], &mut l, &mut r);
    assert!(l.iter().all(|x| *x == 1.0), "(1 + 0) / 2 * 2");
    assert_eq!(l, r);
}

#[test]
fn blocks_longer_than_max_block_are_split_without_losing_events() {
    let mut dev = ClapDevice::load(&info(GAIN), SR, 64).unwrap();
    let input = vec![1.0f32; 200];
    let events = [
        at(
            10,
            EventKind::Param {
                id: GAIN_ID,
                value: 2.0,
            },
        ),
        at(
            130,
            EventKind::Param {
                id: GAIN_ID,
                value: 4.0,
            },
        ),
        at(
            199,
            EventKind::Param {
                id: GAIN_ID,
                value: 0.5,
            },
        ),
    ];
    let (l, _) = run_effect(&mut dev, &events, &input);
    assert!(l[..10].iter().all(|x| *x == 1.0));
    assert!(l[10..130].iter().all(|x| *x == 2.0));
    assert!(l[130..199].iter().all(|x| *x == 4.0));
    assert_eq!(l[199], 0.5);
    assert_eq!(dev.stats().process_errors, 0);
}

// ---- instruments -----------------------------------------------------------

#[test]
fn note_on_makes_sound_on_the_right_frame_and_note_off_stops_it() {
    let mut dev = load(SINE);
    assert!(
        !dev.is_silent(),
        "must be processed once before it can sleep"
    );

    // Key 69 is A4, 440 Hz. Level 0.5 and velocity 1 give amplitude 0.5.
    let (l, r) = run(&mut dev, &[note_on(128, 69, 1)], BLOCK);
    assert!(l[..128].iter().all(|x| *x == 0.0), "silent before the note");
    let expect = sine(440.0, 128..BLOCK, 128, 0.5);
    for (i, (got, want)) in l[128..].iter().zip(&expect).enumerate() {
        assert!(
            (got - want).abs() < 1e-5,
            "frame {}: {got} vs {want}",
            128 + i
        );
    }
    assert_eq!(l, r);
    let peak = l.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!(
        (0.45..=0.5001).contains(&peak),
        "peak {peak}, not the aux port's 0.9"
    );
    assert!(!dev.is_silent());

    // The note ends exactly at frame 64 of the next block.
    let (l, _) = run(&mut dev, &[note_off(64, 69, 1)], BLOCK);
    assert!(l[..64].iter().any(|x| *x != 0.0));
    assert!(l[64..].iter().all(|x| *x == 0.0));
    assert!(dev.is_silent(), "the plugin returned SLEEP with no voices");
    assert_eq!(dev.stats().process_errors, 0);
    assert_eq!(dev.stats().dropped_events, 0);
}

#[test]
fn level_automation_scales_new_notes() {
    let mut dev = load(SINE);
    dev.set_param(LEVEL_ID, 0.25);
    let (l, _) = run(&mut dev, &[note_on(0, 69, 1)], 256);
    let peak = l.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!((0.24..=0.2501).contains(&peak), "peak {peak}");
}

#[test]
fn overlapping_notes_with_the_same_key_release_by_voice() {
    let mut dev = load(SINE);
    let events = [note_on(0, 60, 1), note_on(0, 60, 2), note_off(100, 60, 1)];
    let (l, _) = run(&mut dev, &events, 256);
    assert!(
        l[100..].iter().any(|x| x.abs() > 0.1),
        "voice 2 is still sounding"
    );
    let (l, _) = run(&mut dev, &[note_off(0, 60, 2)], 128);
    assert!(l.iter().all(|x| *x == 0.0));
    assert!(dev.is_silent());
}

#[test]
fn all_notes_off_and_all_sound_off_silence_held_notes() {
    for panic in [EventKind::AllNotesOff, EventKind::AllSoundOff] {
        let mut dev = load(SINE);
        let events = [
            note_on(0, 60, 1),
            note_on(0, 64, 2),
            note_on(0, 67, 3),
            at(100, panic),
        ];
        let (l, _) = run(&mut dev, &events, 256);
        assert!(l[..100].iter().any(|x| x.abs() > 0.1), "{panic:?}");
        assert!(l[100..].iter().all(|x| *x == 0.0), "{panic:?}");
        let (l, _) = run(&mut dev, &[], 128);
        assert!(l.iter().all(|x| *x == 0.0), "{panic:?} left a voice behind");
        assert!(dev.is_silent());
    }
}

#[test]
fn reset_clears_sounding_voices() {
    let mut dev = load(SINE);
    run(&mut dev, &[note_on(0, 60, 1)], 128);
    dev.reset();
    let (l, _) = run(&mut dev, &[], 128);
    assert!(l.iter().all(|x| *x == 0.0));
}

#[test]
fn midi_only_instruments_get_midi_bytes() {
    let mut dev = load(MIDI_SINE);
    let (l, _) = run(&mut dev, &[note_on(32, 69, 1)], 512);
    assert!(l[..32].iter().all(|x| *x == 0.0));
    // MIDI velocity 127 is 127/127 = 1.0 in the plugin; level is 0.5.
    let expect = sine(440.0, 32..512, 32, 0.5);
    for (got, want) in l[32..].iter().zip(&expect) {
        assert!((got - want).abs() < 1e-5);
    }
    let (l, _) = run(&mut dev, &[note_off(10, 69, 1)], 64);
    assert!(l[10..].iter().all(|x| *x == 0.0));

    run(&mut dev, &[note_on(0, 60, 2), note_on(0, 64, 3)], 64);
    let (l, _) = run(&mut dev, &[at(5, EventKind::AllNotesOff)], 64);
    assert!(
        l[5..].iter().all(|x| *x == 0.0),
        "CC 123 releases every note"
    );
}

#[test]
fn voices_continue_across_split_blocks() {
    let mut dev = ClapDevice::load(&info(SINE), SR, 100).unwrap();
    let (l, _) = run(&mut dev, &[note_on(10, 69, 1), note_off(250, 69, 1)], 400);
    let expect = sine(440.0, 10..250, 10, 0.5);
    for (i, (got, want)) in l[10..250].iter().zip(&expect).enumerate() {
        assert!((got - want).abs() < 1e-5, "frame {}", 10 + i);
    }
    assert!(l[250..].iter().all(|x| *x == 0.0));
}

// ---- latency, tail, lifecycle ---------------------------------------------

#[test]
fn latency_and_tail_come_from_the_plugin() {
    let sine_dev = load(SINE);
    assert_eq!(sine_dev.latency(), 32);
    assert_eq!(sine_dev.tail_seconds(), Some(4800.0 / SR));

    let gain = load(GAIN);
    assert_eq!(gain.latency(), 0);
    assert_eq!(gain.tail_seconds(), Some(0.0));

    let midi = load(MIDI_SINE);
    assert_eq!(midi.tail_seconds(), None, "no tail extension means unknown");
}

#[test]
fn prepare_reactivates_for_a_new_rate_and_block_size() {
    let mut dev = load(SINE);
    dev.prepare(SR, BLOCK);
    dev.prepare(44_100.0, 256);
    assert_eq!(dev.tail_seconds(), Some(4800.0 / 44_100.0));
    let (l, _) = run(&mut dev, &[note_on(0, 69, 1)], 256);
    let want = (0.5 * (std::f64::consts::TAU * 440.0 * 100.0 / 44_100.0).sin()) as f32;
    assert!((l[100] - want).abs() < 1e-5, "{} vs {want}", l[100]);

    // Invalid configurations are ignored and leave the plugin usable.
    dev.prepare(0.0, 64);
    dev.prepare(48_000.0, 0);
    let (l, _) = run(&mut dev, &[], 64);
    assert!(l.iter().any(|x| *x != 0.0));
}

#[test]
fn defaults_apply_through_the_device_trait() {
    let mut dev = load(GAIN);
    dev.set_param(GAIN_ID, 3.0);
    peel_dsp::device::apply_defaults(&mut dev);
    assert_eq!(dev.param(GAIN_ID), 1.0);
    assert_eq!(dev.param(INVERT_ID), 0.0);
    let (l, _) = run_effect(&mut dev, &[], &[0.5; 16]);
    assert_eq!(l, [0.5; 16]);
}

// ---- state -------------------------------------------------------------------

#[test]
fn state_round_trips_between_instances() {
    let mut a = load(GAIN);
    a.set_param(GAIN_ID, 2.5);
    a.set_param(INVERT_ID, 1.0);
    run_effect(&mut a, &[], &[0.0; 8]);
    let blob = a.save_state().expect("save state");
    assert!(blob.starts_with(b"PTS1"));

    let mut b = load(GAIN);
    assert_eq!(b.param(GAIN_ID), 1.0);
    b.load_state(&blob).expect("load state");
    assert_eq!(
        b.param(GAIN_ID),
        2.5,
        "the parameter mirror follows the state"
    );
    assert_eq!(b.param(INVERT_ID), 1.0);
    let (l, _) = run_effect(&mut b, &[], &[0.5; 8]);
    assert_eq!(l, [-1.25; 8]);
}

#[test]
fn parameters_reach_the_plugin_without_audio_running() {
    // Nothing has been processed: set_param goes through params.flush, so
    // the saved state already has the change.
    let dev = {
        let mut dev = load(GAIN);
        dev.set_param(GAIN_ID, 0.25);
        dev.set_param(INVERT_ID, 1.0);
        dev
    };
    let blob = dev.handle().save_state().unwrap();
    let mut other = load(GAIN);
    other.load_state(&blob).unwrap();
    assert_eq!(other.param(GAIN_ID), 0.25);
    assert_eq!(other.param(INVERT_ID), 1.0);

    // The same holds for a plugin that has gone to sleep.
    let mut sine = load(SINE);
    run(&mut sine, &[note_on(0, 60, 1), note_off(10, 60, 1)], 64);
    assert!(sine.is_silent());
    sine.set_param(LEVEL_ID, 0.125);
    let blob = sine.handle().save_state().unwrap();
    let mut other = load(SINE);
    other.load_state(&blob).unwrap();
    assert_eq!(other.param(LEVEL_ID), 0.125);
    // And the next note uses it.
    let (l, _) = run(&mut sine, &[note_on(0, 69, 2)], 256);
    let peak = l.iter().fold(0.0f32, |m, x| m.max(x.abs()));
    assert!((0.12..=0.1251).contains(&peak), "peak {peak}");
}

#[test]
fn events_past_the_end_of_the_block_are_delivered_on_its_last_frame() {
    let mut dev = load(GAIN);
    let events = [at(
        9_999,
        EventKind::Param {
            id: GAIN_ID,
            value: 0.5,
        },
    )];
    let (l, _) = run_effect(&mut dev, &events, &[1.0; 16]);
    assert!(l[..15].iter().all(|x| *x == 1.0));
    assert_eq!(l[15], 0.5);
}

#[test]
fn restoring_state_discards_changes_still_waiting_for_the_next_block() {
    let mut a = load(GAIN);
    a.set_param(GAIN_ID, 2.0);
    let blob = a.save_state().unwrap();

    // B is processing, so its set_param waits for the next block.
    let mut b = load(GAIN);
    run_effect(&mut b, &[], &[0.0; 4]);
    b.set_param(GAIN_ID, 4.0);
    assert_eq!(b.param(GAIN_ID), 4.0);
    b.load_state(&blob).unwrap();
    assert_eq!(b.param(GAIN_ID), 2.0);
    let (l, _) = run_effect(&mut b, &[], &[0.5; 8]);
    assert_eq!(l, [1.0; 8], "the restored 2.0 wins over the queued 4.0");
}

#[test]
fn bad_state_is_rejected_without_breaking_the_plugin() {
    let mut dev = load(GAIN);
    dev.set_param(GAIN_ID, 2.0);
    run_effect(&mut dev, &[], &[0.0; 4]);
    for bad in [&b""[..], b"PTS1", b"nope-nope-nope", &[0u8; 64][..]] {
        let err = dev.load_state(bad).expect_err("must be rejected");
        assert!(matches!(err, ClapError::State(_)), "{err}");
    }
    let (l, _) = run_effect(&mut dev, &[], &[0.5; 4]);
    assert_eq!(l, [1.0; 4], "gain 2.0 is untouched");
}

#[test]
fn state_goes_through_the_handle_while_the_device_is_in_use() {
    let mut dev = load(GAIN);
    let handle = dev.handle();
    assert!(handle.can_save_state() && handle.has_gui());
    dev.set_param(GAIN_ID, 3.0);
    run_effect(&mut dev, &[], &[0.0; 4]);

    // The device would be on the audio thread; the handle works from here.
    let blob = std::thread::scope(|s| {
        let audio = s.spawn(|| {
            for _ in 0..50 {
                run_effect(&mut dev, &[], &[0.25; 64]);
            }
        });
        let blob = handle.save_state().expect("save while processing");
        audio.join().unwrap();
        blob
    });
    let fresh = load(GAIN);
    fresh
        .handle()
        .load_state(&blob)
        .expect("load through handle");
    assert_eq!(fresh.handle().param(GAIN_ID), Some(3.0));
    assert_eq!(handle.param(GAIN_ID), Some(3.0));
    assert_eq!(handle.param(31337), None);
}

// ---- host callbacks and threads ---------------------------------------------

#[test]
fn param_text_uses_the_plugin_formatter() {
    let dev = load(GAIN);
    let handle = dev.handle();
    assert_eq!(handle.param_text(GAIN_ID, 2.0).as_deref(), Some("2.00x"));
    assert_eq!(
        handle.param_text(INVERT_ID, 1.0).as_deref(),
        Some("Inverted")
    );
    assert_eq!(handle.param_from_text(GAIN_ID, "1.5x"), Some(1.5));
    assert_eq!(handle.param_from_text(GAIN_ID, "loud"), None);
}

#[test]
fn plugin_requests_are_serviced_by_idle() {
    let mut dev = load(GAIN);
    let handle = dev.handle();
    let quiet = handle.idle();
    assert_eq!(quiet, peel_host_clap::IdleReport::default());

    // Gain 3.5 makes the fixture ask for a main-thread callback, as if its
    // own GUI had moved a knob. Nothing happens until idle runs.
    let events = [at(
        0,
        EventKind::Param {
            id: GAIN_ID,
            value: 3.5,
        },
    )];
    let (l, _) = run_effect(&mut dev, &events, &[1.0; 16]);
    assert_eq!(l, [1.0; 16]);
    assert_eq!(handle.latency(), 0);
    assert_eq!(
        handle.param(GAIN_ID),
        Some(3.5),
        "mirror shows the host's last write"
    );

    let report = handle.idle();
    assert!(report.callback_serviced);
    assert!(report.latency_changed);
    assert!(report.params_changed);
    assert!(report.state_dirty);
    assert!(report.restart_requested);
    assert!(!report.param_layout_changed && !report.ports_changed);
    assert_eq!(handle.latency(), 64);
    assert_eq!(dev.latency(), 64);
    assert_eq!(
        handle.param(GAIN_ID),
        Some(0.75),
        "rescan pulled the plugin's value"
    );

    // Restart stays pending until the device is prepared again.
    let again = handle.idle();
    assert!(again.restart_requested && !again.callback_serviced && !again.state_dirty);
    dev.prepare(SR, BLOCK);
    assert!(!handle.idle().restart_requested);
    let (l, _) = run_effect(&mut dev, &[], &[1.0; 16]);
    assert_eq!(l, [0.75; 16], "the plugin kept its own change");
}

#[test]
fn thread_roles_are_correct_in_every_call() {
    // The fixture asks `clap.thread-check` on each call and fails the call
    // when the role is wrong: process errors, a failed save, a failed load.
    let mut dev = load(GAIN);
    let handle = dev.handle();
    let events = [at(
        0,
        EventKind::Param {
            id: GAIN_ID,
            value: 0.5,
        },
    )];
    for _ in 0..4 {
        run_effect(&mut dev, &events, &[0.5; 64]);
    }
    dev.reset();
    handle.idle();
    handle.param_text(GAIN_ID, 1.0);
    dev.prepare(44_100.0, 128);
    run_effect(&mut dev, &[], &[0.5; 64]);
    // From another thread, as the UI would.
    let blob = std::thread::spawn({
        let handle = handle.clone();
        move || {
            handle.idle();
            handle.save_state()
        }
    })
    .join()
    .unwrap()
    .expect("state can be saved from any control thread");
    handle.load_state(&blob).expect("load");
    assert_eq!(dev.stats().process_errors, 0);
    // The fixture remembers a wrong-thread call in `reset`, `deactivate` and
    // `on_main_thread`, and refuses to save once it has seen one.
    assert!(handle.save_state().is_ok());
}

#[test]
fn render_mode_is_forwarded_when_the_plugin_has_the_extension() {
    assert!(load(GAIN).set_offline_render(true));
    assert!(!load(SINE).set_offline_render(true));
    assert!(!load(GAIN).handle().requires_realtime());
}

#[test]
fn the_plugin_outlives_the_device_while_a_handle_exists() {
    let dev = load(GAIN);
    let handle = dev.handle();
    drop(dev);
    assert!(handle.save_state().is_ok());
    drop(handle);
}

#[test]
fn many_devices_share_one_loaded_library() {
    let devices: Vec<ClapDevice> = (0..6)
        .map(|i| load(if i % 2 == 0 { GAIN } else { SINE }))
        .collect();
    drop(devices);
    // Loading again after everything was released still works.
    let mut dev = load(GAIN);
    let (l, _) = run_effect(&mut dev, &[], &[0.5; 4]);
    assert_eq!(l, [0.5; 4]);
}

#[test]
fn the_plugin_window_opens_resizes_on_request_and_closes() {
    let dev = load(GAIN);
    let handle = dev.handle();
    if !cfg!(windows) {
        assert!(
            !handle.has_editor(),
            "plugin windows are Windows-only for now"
        );
        assert!(handle.open_editor().is_err());
        return;
    }
    assert!(handle.has_editor());
    assert!(!handle.editor_open());
    handle.open_editor().expect("open the window");
    assert!(handle.editor_open());
    // The fixture asks to grow right after it is attached; idle applies it
    // without disturbing anything else.
    let _ = handle.idle();
    assert!(handle.editor_open());
    // Opening again only brings the window forward.
    handle.open_editor().expect("raise");
    handle.close_editor();
    assert!(!handle.editor_open());
    // It can be opened again after closing, and dropping the plugin with the
    // window open tears both down cleanly.
    handle.open_editor().expect("reopen");
    drop(handle);
    drop(dev);
}

#[test]
fn plugins_without_a_win32_window_report_no_editor() {
    let handle = load(SINE).handle();
    assert!(!handle.has_editor());
    assert!(handle.open_editor().is_err());
}
