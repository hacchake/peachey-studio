use std::sync::atomic::Ordering;

use peel_midi_io::MidiEvent;

use super::*;
use crate::template;

fn session() -> Session {
    Session::new_silent(template::beat_and_melody())
}

fn channel(s: &Session, name: &str) -> Id {
    s.project()
        .channels
        .iter()
        .find(|c| c.name == name)
        .expect("channel")
        .id
}

fn all_channels(s: &Session) -> Vec<Id> {
    s.project().channels.iter().map(|c| c.id).collect()
}

/// Runs the session's per-frame upkeep until `cond` holds.
fn wait_for(s: &mut Session, what: &str, mut cond: impl FnMut(&mut Session) -> bool) {
    let start = Instant::now();
    loop {
        s.tick();
        if cond(s) {
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(3));
    }
}

fn sounds(s: &mut Session, ch: Id) -> bool {
    s.take_channel_activity(ch) > 0.0
}

fn note_on(key: u8, velocity: u8) -> MidiEvent {
    MidiEvent::NoteOn {
        channel: 0,
        key,
        velocity,
    }
}

fn note_off(key: u8) -> MidiEvent {
    MidiEvent::NoteOff {
        channel: 0,
        key,
        velocity: 0,
    }
}

#[test]
fn restart_reinstalls_every_device_and_resends_the_plan() {
    let mut s = session();
    let channels = all_channels(&s);
    let before = s.devices.len();
    assert!(before >= channels.len());
    assert_eq!(s.buffer_frames(), 256);

    // Before: playing a note on any instrument sounds.
    for &ch in &channels {
        s.live_note(ch, 60, 1.0, true);
        wait_for(&mut s, "a note before the restart", |s| sounds(s, ch));
        s.live_note(ch, 60, 0.0, false);
    }

    s.restart_audio(None, Some(128)).expect("restart");
    assert_eq!(s.buffer_frames(), 128);
    assert_eq!(s.audio_device_name(), "No audio device");
    assert_eq!(s.devices.len(), before, "no slots were lost or invented");
    assert!(!s.is_playing());

    // The plan arrived with the devices: the starter beat has a kick on the
    // first beat, and the master hears it.
    s.play();
    wait_for(&mut s, "the plan to play", |s| {
        s.telemetry().take_master().0 > 0.0
    });
    wait_for(&mut s, "the playhead to move", |s| s.position() > 0.25);
    s.stop();

    // And every instrument is installed again.
    for &ch in &channels {
        s.live_note(ch, 60, 1.0, true);
        wait_for(&mut s, "a note after the restart", |s| sounds(s, ch));
        s.live_note(ch, 60, 0.0, false);
    }
    let t = s.telemetry();
    assert_eq!(t.plans_rejected.load(Ordering::Relaxed), 0);
    assert_eq!(t.retire_leaks.load(Ordering::Relaxed), 0);
}

#[test]
fn restart_keeps_the_playhead_and_a_changed_sample_rate_works() {
    let mut s = session();
    s.seek(6.0);
    wait_for(&mut s, "the seek", |s| (s.position() - 6.0).abs() < 1e-6);
    s.set_metronome(true);

    let wanted = EngineConfig {
        sample_rate: Some(44_100),
        buffer_frames: Some(512),
        ..s.io.config.clone()
    };
    s.restart_with(wanted).expect("restart");
    wait_for(&mut s, "the playhead to come back", |s| {
        (s.position() - 6.0).abs() < 1e-6
    });
    assert_eq!(s.sample_rate(), 44_100.0);
    assert_eq!(s.buffer_frames(), 512);
    assert!(s.metronome());

    // Devices were prepared for the new rate and work.
    let piano = channel(&s, "Piano");
    s.live_note(piano, 60, 1.0, true);
    wait_for(&mut s, "a note at 44.1 kHz", |s| sounds(s, piano));
}

#[test]
fn an_empty_device_name_means_the_default_and_the_request_is_remembered() {
    let mut s = session();
    s.restart_audio(Some("  ".into()), None).unwrap();
    assert_eq!(s.requested_audio_device(), None);
    s.restart_audio(Some("Some Interface".into()), Some(64))
        .unwrap();
    // The silent clock has no devices; the request is still recorded.
    assert_eq!(s.requested_audio_device(), Some("Some Interface"));
    assert_eq!(s.buffer_frames(), 64);
}

#[test]
fn midi_notes_play_the_selected_channel_without_the_ui() {
    let mut s = session();
    let piano = channel(&s, "Piano");
    let kick = channel(&s, "Kick");

    // No target: ignored.
    s.io.bridge.handle(&note_on(60, 100));
    assert_eq!(s.midi_activity(), 0);

    s.set_live_target(Some(piano));
    assert_eq!(s.live_target(), Some(piano));
    s.io.bridge.handle(&note_on(60, 100));
    wait_for(&mut s, "the piano to sound", |s| sounds(s, piano));
    assert!(!sounds(&mut s, kick), "only the selected channel plays");
    assert_eq!(s.midi_activity(), 1);

    // The note is recorded with its channel and velocity.
    s.poll_midi();
    let notes = s.take_recorded_midi();
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].channel, Some(piano));
    assert!(notes[0].on && notes[0].key == 60);
    assert!((notes[0].velocity - 100.0 / 127.0).abs() < 1e-6);
    assert!(!notes[0].playing && !notes[0].sustain);
    assert!(s.take_recorded_midi().is_empty());

    // Switching channels while holding the key: the release still ends the
    // piano's note, and the next note goes to the new channel.
    s.set_live_target(Some(kick));
    s.io.bridge.handle(&note_off(60));
    s.io.bridge.handle(&note_on(62, 90));
    wait_for(&mut s, "the kick to sound", |s| sounds(s, kick));

    s.set_live_target(None);
    s.io.bridge.handle(&note_on(64, 90));
    assert_eq!(s.midi_activity(), 2, "no target, no note");
}

#[test]
fn the_pedal_and_all_notes_off_reach_the_engine() {
    let mut s = session();
    let piano = channel(&s, "Piano");
    s.set_live_target(Some(piano));
    let initial = s.engine.live_stats().events();
    s.io.bridge.handle(&MidiEvent::Sustain {
        channel: 0,
        down: true,
    });
    s.io.bridge.handle(&note_on(60, 100));
    s.io.bridge.handle(&note_off(60));
    wait_for(&mut s, "the piano to sound", |s| sounds(s, piano));
    // The pedal is two events: the host's hold and CC64 for the instrument.
    assert_eq!(s.engine.live_stats().events(), initial + 4);

    s.io.bridge.handle(&MidiEvent::AllNotesOff { channel: 0 });
    assert_eq!(s.engine.live_stats().events(), initial + 5);
    let notes = s.take_recorded_midi();
    assert_eq!(notes.len(), 2);
    assert!(notes[1].sustain, "the release was recorded under the pedal");
}

#[test]
fn midi_keeps_working_after_an_audio_restart() {
    let mut s = session();
    let piano = channel(&s, "Piano");
    s.set_live_target(Some(piano));
    s.io.bridge.handle(&note_on(60, 100));
    wait_for(&mut s, "the piano to sound", |s| sounds(s, piano));

    s.restart_audio(None, Some(128)).unwrap();
    // The target moved to the new engine.
    assert_eq!(s.live_target(), Some(piano));
    s.io.bridge.handle(&note_on(64, 100));
    wait_for(&mut s, "the piano to sound again", |s| sounds(s, piano));

    // Notes from before the restart were kept for recording.
    let notes = s.take_recorded_midi();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!((notes[0].key, notes[1].key), (60, 64));
}

#[test]
fn spread_timing_still_plays_every_note_promptly() {
    let mut s = session();
    let piano = channel(&s, "Piano");
    s.set_live_target(Some(piano));
    s.set_midi_timing(LiveTiming::Spread);
    // Notes arrive at arbitrary points in the audio callbacks; each is heard
    // within a couple of blocks and none is lost.
    for key in 40..60u8 {
        s.io.bridge.handle(&note_on(key, 100));
        wait_for(&mut s, "the note to sound", |s| sounds(s, piano));
        s.io.bridge.handle(&note_off(key));
        std::thread::sleep(Duration::from_millis(u64::from(key) % 7));
    }
    assert_eq!(s.midi_activity(), 20);
    assert_eq!(s.engine.live_stats().dropped(), 0);
}

#[test]
fn silent_sessions_do_not_open_hardware_and_polling_is_harmless() {
    let mut s = session();
    assert!(!s.midi_enabled());
    assert!(s.midi_inputs().is_empty());
    for _ in 0..3 {
        s.poll_midi();
    }
    assert_eq!(s.midi_message_count(), 0);
    s.select_midi_input(PortSelection::Nothing);
    s.set_midi_enabled(true);
    assert!(s.midi_enabled());
    s.poll_midi();
    s.set_midi_enabled(false);
    assert!(s.midi_inputs().is_empty());
    s.set_midi_timing(LiveTiming::Spread);
    assert_eq!(s.midi_timing(), LiveTiming::Spread);
}

#[test]
#[ignore = "needs an audio output device"]
fn restarting_on_the_real_device_keeps_the_sound() {
    let mut s = Session::new(template::beat_and_melody());
    if s.engine.is_null_backend() {
        return;
    }
    let piano = channel(&s, "Piano");
    s.restart_audio(None, Some(512)).expect("restart");
    assert!(!s.engine.is_null_backend());
    s.live_note(piano, 60, 1.0, true);
    wait_for(&mut s, "the piano to sound", |s| sounds(s, piano));
    // An unknown device falls back to the default and says so.
    s.restart_audio(Some("No Such Interface".into()), None)
        .expect("fallback");
    assert!(s.audio_warning().is_some());
    assert!(!s.engine.is_null_backend());

    // MIDI notes reach the device callback in both timing modes.
    s.set_live_target(Some(piano));
    for timing in [LiveTiming::Immediate, LiveTiming::Spread] {
        s.set_midi_timing(timing);
        for key in 50..56u8 {
            s.io.bridge.handle(&note_on(key, 110));
            wait_for(&mut s, "a MIDI note on the device", |s| sounds(s, piano));
            s.io.bridge.handle(&note_off(key));
            std::thread::sleep(Duration::from_millis(15));
        }
    }
    assert_eq!(s.engine.live_stats().dropped(), 0);
    assert_eq!(
        s.telemetry().overruns.load(Ordering::Relaxed),
        0,
        "no missed deadlines"
    );
}

/// A virtual MIDI keyboard stands in for hardware: its bytes travel through
/// the real system MIDI layer, the port thread, the bridge and the engine's
/// live queue to the instrument, and the time is printed.
#[cfg(unix)]
#[test]
#[ignore = "needs the system MIDI service (CoreMIDI or the ALSA sequencer)"]
fn a_midi_keyboard_plays_the_selected_channel_through_the_session() {
    use midir::os::unix::VirtualOutput;

    let name = "Peel Session Test Keyboard";
    let mut keyboard = midir::MidiOutput::new("Peel session test")
        .expect("MIDI output")
        .create_virtual(name)
        .unwrap_or_else(|_| panic!("could not create a virtual port"));

    let mut s = session();
    let piano = channel(&s, "Piano");
    s.set_midi_enabled(true);
    s.set_live_target(Some(piano));
    wait_for(&mut s, "the keyboard to connect", |s| {
        s.poll_midi();
        s.midi_inputs().iter().any(|n| n.contains(name))
    });
    assert!(s.notice().is_some_and(|n| n.text.contains("connected")));

    let sent = Instant::now();
    keyboard.send(&[0x90, 60, 100]).unwrap();
    wait_for(&mut s, "the piano to sound", |s| sounds(s, piano));
    eprintln!(
        "key press to sound on the silent clock: {:?}",
        sent.elapsed()
    );
    assert_eq!(s.midi_activity(), 1);

    keyboard.send(&[0xB0, 64, 127]).unwrap();
    keyboard.send(&[0x80, 60, 0]).unwrap();
    wait_for(&mut s, "the pedal and release", |s| {
        s.engine.live_stats().events() >= 3
    });
    let notes = s.take_recorded_midi();
    assert_eq!(notes.len(), 2);
    assert!(!notes[1].on && notes[1].sustain);

    // Unplugging is noticed.
    drop(keyboard);
    wait_for(&mut s, "the keyboard to disconnect", |s| {
        s.poll_midi();
        !s.midi_inputs().iter().any(|n| n.contains(name))
    });
}

#[test]
fn expression_accepts_hardware_and_software_with_one_filter_and_readback() {
    let mut s = session();
    assert!(!s.pitch_bend(1.0));
    assert!(!s.channel_pressure(0.5));
    assert!(!s.reset_expression());
    s.set_pitch_bend_range(12.5).unwrap();
    assert_eq!(s.pitch_bend_range(), 12.5);
    let piano = channel(&s, "Piano");
    s.set_live_target(Some(piano));
    s.set_midi_channel_filter(Some(2)).unwrap();
    s.handle_midi_event(MidiEvent::PitchBend {
        channel: 1,
        value: 8191,
    });
    assert_eq!(s.pitch_bend_value(), 0.0);
    s.handle_midi_event(MidiEvent::PitchBend {
        channel: 2,
        value: -8192,
    });
    assert_eq!(s.pitch_bend_value(), -1.0);
    s.io.bridge.hardware_enabled.store(true, Ordering::Release);
    s.io.bridge.handle_hardware(&MidiEvent::PitchBend {
        channel: 2,
        value: 8191,
    });
    assert_eq!(s.pitch_bend_value(), 1.0);
    s.io.bridge.handle_hardware(&MidiEvent::ChannelPressure {
        channel: 2,
        value: 127,
    });
    s.handle_midi_event(MidiEvent::PolyPressure {
        channel: 2,
        key: 60,
        value: 64,
    });
    assert_eq!(s.channel_pressure_value(), 1.0);
    assert_eq!(s.poly_pressure_value(60), 64.0 / 127.0);
    s.handle_midi_event(MidiEvent::NoteOn {
        channel: 2,
        key: 60,
        velocity: 100,
    });
    assert_eq!(s.poly_pressure_value(60), 0.0);
    assert_eq!(s.pitch_bend_value(), 1.0);
    assert_eq!(s.channel_pressure_value(), 1.0);
    assert!(s.poly_pressure(60, 0.75));
    s.live_note(piano, 60, 1.0, true);
    assert_eq!(s.poly_pressure_value(60), 0.0);
    assert!(!s.pitch_bend(f32::NAN));
    assert!(!s.channel_pressure(f32::INFINITY));
    assert!(!s.poly_pressure(128, 0.5));
    assert!(s.set_pitch_bend_range(49.0).is_err());
    assert!(s.set_pitch_bend_range(f32::NAN).is_err());
    assert_eq!(s.pitch_bend_range(), 12.5);
    assert!(s.reset_expression());
    assert_eq!(s.pitch_bend_value(), 0.0);
    assert_eq!(s.channel_pressure_value(), 0.0);
    assert_eq!(s.poly_pressure_value(60), 0.0);
    assert_eq!(s.pitch_bend_range(), 12.5);
    assert!(s.pitch_bend(0.5));
    s.set_midi_channel_filter(None).unwrap();
    assert_eq!(s.pitch_bend_value(), 0.0);
    assert!(s.channel_pressure(0.8));
    s.panic();
    assert_eq!(s.channel_pressure_value(), 0.0);
    assert!(s.pitch_bend(-0.5));
    s.set_live_target(None);
    assert_eq!(s.pitch_bend_value(), 0.0);
    s.set_live_target(Some(piano));
    assert!(s.pitch_bend(0.4));
    s.restart_audio(None, Some(128)).unwrap();
    assert_eq!(s.pitch_bend_value(), 0.0);
    assert_eq!(s.pitch_bend_range(), 12.5);
}

fn mpe_session(mode: MpeMode, members: u8) -> Session {
    let mut s = session();
    let piano = channel(&s, "Piano");
    s.set_live_target(Some(piano));
    s.configure_mpe(MpeConfig {
        mode,
        members,
        ..Default::default()
    })
    .unwrap();
    s.take_recorded_performance();
    s
}
fn mpe_on(s: &mut Session, ch: u8, key: u8) {
    s.handle_midi_event(MidiEvent::NoteOn {
        channel: ch,
        key,
        velocity: 100,
    });
}
fn mpe_off(s: &mut Session, ch: u8, key: u8) {
    s.handle_midi_event(MidiEvent::NoteOff {
        channel: ch,
        key,
        velocity: 0,
    });
}
fn mpe_cc(s: &mut Session, ch: u8, cc: u8, value: u8) {
    s.handle_midi_event(MidiEvent::ControlChange {
        channel: ch,
        controller: cc,
        value,
    });
}
fn mpe_expression(events: &[RecordedPerformance], voice: u32) -> (f32, f32, f32) {
    events
        .iter()
        .rev()
        .find_map(|e| match e.kind {
            RecordedPerformanceKind::Expression {
                pitch,
                pressure,
                timbre,
            } if e.voice == voice => Some((pitch, pressure, timbre)),
            _ => None,
        })
        .expect("expression for voice")
}
#[test]
fn mpe_same_key_on_different_members_keeps_independent_voice_identity() {
    let mut s = mpe_session(MpeMode::Lower, 4);
    s.set_midi_channel_filter(Some(12)).unwrap();
    let before = s.project().clone();
    mpe_on(&mut s, 1, 60);
    mpe_on(&mut s, 2, 60);
    let starts = s.take_recorded_performance();
    let notes: Vec<_> = starts
        .iter()
        .filter(|e| matches!(e.kind, RecordedPerformanceKind::NoteOn { .. }))
        .collect();
    assert_eq!(notes.len(), 2);
    assert_ne!(notes[0].voice, notes[1].voice);
    let (a, b) = (notes[0].voice, notes[1].voice);
    assert_eq!(notes[0].midi_channel, Some(1));
    assert_eq!(notes[1].midi_channel, Some(2));
    s.handle_midi_event(MidiEvent::PitchBend {
        channel: 1,
        value: 8191,
    });
    let bent = s.take_recorded_performance();
    assert_eq!(mpe_expression(&bent, a), (48.0, 0.0, 0.5));
    assert!(bent.iter().all(|e| e.voice != b));
    s.handle_midi_event(MidiEvent::ChannelPressure {
        channel: 2,
        value: 127,
    });
    mpe_cc(&mut s, 2, 74, 127);
    let pressed = s.take_recorded_performance();
    assert_eq!(mpe_expression(&pressed, b), (0.0, 1.0, 1.0));
    assert!(pressed.iter().all(|e| e.voice != a));
    mpe_off(&mut s, 1, 60);
    let off = s.take_recorded_performance();
    assert!(off.iter().any(
        |e| e.voice == a && matches!(e.kind, RecordedPerformanceKind::NoteOff { key: 60, .. })
    ));
    assert!(off.iter().all(|e| e.voice != b));
    assert_eq!(s.project(), &before);
}
#[test]
fn mpe_manager_and_pre_note_member_state_are_combined_once_and_read_back() {
    let mut s = mpe_session(MpeMode::Lower, 4);
    s.handle_midi_event(MidiEvent::PitchBend {
        channel: 1,
        value: 8191,
    });
    s.handle_midi_event(MidiEvent::PitchBend {
        channel: 0,
        value: 8191,
    });
    s.handle_midi_event(MidiEvent::ChannelPressure {
        channel: 1,
        value: 100,
    });
    s.handle_midi_event(MidiEvent::ChannelPressure {
        channel: 0,
        value: 63,
    });
    mpe_cc(&mut s, 1, 74, 127);
    mpe_on(&mut s, 1, 60);
    let events = s.take_recorded_performance();
    let on = events
        .iter()
        .find(|e| matches!(e.kind, RecordedPerformanceKind::NoteOn { .. }))
        .unwrap();
    assert_eq!(mpe_expression(&events, on.voice), (50.0, 1.0, 1.0));
    assert_eq!(s.pitch_bend_value(), 1.0);
    assert!((s.channel_pressure_value() - 63.0 / 127.0).abs() < 1e-6);
    assert!(s.pitch_bend(-1.0));
    assert_eq!(
        mpe_expression(&s.take_recorded_performance(), on.voice).0,
        46.0
    );
    s.set_pitch_bend_range(12.0).unwrap();
    assert_eq!(s.mpe_config().manager_bend_range, 12.0);
    assert_eq!(s.pitch_bend_range(), 12.0);
    assert_eq!(
        mpe_expression(&s.take_recorded_performance(), on.voice).0,
        36.0
    );
}
#[test]
fn mpe_upper_zone_rpn_sensitivity_and_zone_resize_follow_the_selected_manager() {
    let mut s = mpe_session(MpeMode::Upper, 3);
    mpe_on(&mut s, 14, 60);
    mpe_on(&mut s, 13, 60);
    mpe_on(&mut s, 1, 60);
    mpe_on(&mut s, 15, 60);
    let events = s.take_recorded_performance();
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e.kind, RecordedPerformanceKind::NoteOn { .. }))
            .count(),
        2
    );
    for (cc, value) in [
        (101, 0),
        (100, 0),
        (6, 12),
        (38, 50),
        (101, 127),
        (100, 127),
    ] {
        mpe_cc(&mut s, 14, cc, value);
    }
    assert_eq!(s.mpe_config().member_bend_range, 12.5);
    s.handle_midi_event(MidiEvent::PitchBend {
        channel: 14,
        value: -8192,
    });
    let events = s.take_recorded_performance();
    assert!(
        events.iter().any(
            |e| matches!(e.kind,RecordedPerformanceKind::Expression{pitch,..} if pitch == -12.5)
        )
    );
    for (cc, value) in [(101, 0), (100, 6), (6, 2), (101, 127), (100, 127)] {
        mpe_cc(&mut s, 15, cc, value);
    }
    assert_eq!(s.mpe_config().members, 2);
    mpe_on(&mut s, 12, 62);
    assert!(
        s.take_recorded_performance()
            .iter()
            .all(|e| !matches!(e.kind, RecordedPerformanceKind::NoteOn { key: 62, .. }))
    );
    for (cc, value) in [(101, 0), (100, 6), (6, 0)] {
        mpe_cc(&mut s, 15, cc, value);
    }
    assert_eq!(s.mpe_config().mode, MpeMode::Off);
}
#[test]
fn mpe_sustain_and_channel_reuse_preserve_the_other_same_pitch_voice() {
    let mut s = mpe_session(MpeMode::Lower, 4);
    mpe_on(&mut s, 1, 60);
    mpe_on(&mut s, 2, 60);
    let initial = s.take_recorded_performance();
    let voices: Vec<_> = initial
        .iter()
        .filter(|e| matches!(e.kind, RecordedPerformanceKind::NoteOn { .. }))
        .map(|e| e.voice)
        .collect();
    s.handle_midi_event(MidiEvent::Sustain {
        channel: 0,
        down: true,
    });
    mpe_off(&mut s, 1, 60);
    mpe_off(&mut s, 2, 60);
    assert!(
        !s.take_recorded_performance()
            .iter()
            .any(|e| matches!(e.kind, RecordedPerformanceKind::NoteOff { .. }))
    );
    s.handle_midi_event(MidiEvent::PitchBend {
        channel: 1,
        value: 8191,
    });
    assert_eq!(
        mpe_expression(&s.take_recorded_performance(), voices[0]).0,
        48.0
    );
    mpe_on(&mut s, 1, 60);
    let retrigger = s.take_recorded_performance();
    assert!(
        retrigger
            .iter()
            .any(|e| e.voice == voices[0]
                && matches!(e.kind, RecordedPerformanceKind::NoteOff { .. }))
    );
    let next = retrigger
        .iter()
        .find(|e| matches!(e.kind, RecordedPerformanceKind::NoteOn { .. }))
        .unwrap()
        .voice;
    assert_ne!(next, voices[0]);
    assert!(retrigger.iter().all(|e| e.voice != voices[1]));
    s.handle_midi_event(MidiEvent::Sustain {
        channel: 0,
        down: false,
    });
    let released = s.take_recorded_performance();
    assert!(
        released
            .iter()
            .any(|e| e.voice == voices[1]
                && matches!(e.kind, RecordedPerformanceKind::NoteOff { .. }))
    );
    assert!(
        !released
            .iter()
            .any(|e| e.voice == next && matches!(e.kind, RecordedPerformanceKind::NoteOff { .. }))
    );
}
#[test]
fn mpe_lifecycle_emits_a_recording_boundary_and_configuration_is_validated_first() {
    let mut s = mpe_session(MpeMode::Lower, 3);
    let before = s.mpe_config();
    for invalid in [
        MpeConfig {
            members: 0,
            ..before
        },
        MpeConfig {
            members: 16,
            ..before
        },
        MpeConfig {
            member_bend_range: f32::NAN,
            ..before
        },
        MpeConfig {
            manager_bend_range: 97.0,
            ..before
        },
    ] {
        assert!(s.configure_mpe(invalid).is_err());
        assert_eq!(s.mpe_config(), before);
    }
    mpe_on(&mut s, 1, 60);
    s.take_recorded_performance();
    let bass = channel(&s, "Bass");
    s.set_live_target(Some(bass));
    assert!(
        s.take_recorded_performance()
            .iter()
            .any(|e| matches!(e.kind, RecordedPerformanceKind::Reset))
    );
    mpe_on(&mut s, 1, 62);
    let events = s.take_recorded_performance();
    let voice = events
        .iter()
        .find(|e| matches!(e.kind, RecordedPerformanceKind::NoteOn { .. }))
        .unwrap()
        .voice;
    assert_eq!(mpe_expression(&events, voice), (0.0, 0.0, 0.5));
    s.panic();
    assert!(
        s.take_recorded_performance()
            .iter()
            .any(|e| matches!(e.kind, RecordedPerformanceKind::Reset))
    );
}
#[test]
fn mpe_member_slide_is_not_sent_to_the_cc_mapper() {
    let mut s = mpe_session(MpeMode::Lower, 3);
    let device = s
        .project()
        .channel(s.live_target().unwrap())
        .unwrap()
        .device
        .clone();
    let param = peel_devices::descriptor(&device.type_id).unwrap().params[0];
    s.set_midi_control_mappings(vec![MidiMapping {
        controller: 74,
        channel: None,
        target: MidiControlTarget::DeviceParam {
            device: device.id,
            key: param.key.into(),
        },
        min: param.min,
        max: param.max,
    }])
    .unwrap();
    let before = s.project().clone();
    mpe_on(&mut s, 1, 60);
    mpe_cc(&mut s, 1, 74, 127);
    s.poll_midi();
    assert_eq!(s.project(), &before);
}
#[test]
fn legacy_and_internal_notes_record_initial_state_and_controllers_in_the_same_feed() {
    let mut s = session();
    let piano = channel(&s, "Piano");
    s.set_live_target(Some(piano));
    s.take_recorded_performance();
    s.set_pitch_bend_range(12.0).unwrap();
    assert!(s.pitch_bend(0.5));
    s.live_note(piano, 60, 0.8, true);
    let events = s.take_recorded_performance();
    let voice = events
        .iter()
        .find(|e| matches!(e.kind, RecordedPerformanceKind::NoteOn { .. }))
        .unwrap()
        .voice;
    assert_eq!(mpe_expression(&events, voice), (6.0, 0.0, 0.5));
    assert!(s.channel_pressure(0.5));
    assert!(s.poly_pressure(60, 0.5));
    assert_eq!(
        mpe_expression(&s.take_recorded_performance(), voice),
        (6.0, 0.75, 0.5)
    );
    s.handle_midi_event(MidiEvent::Sustain {
        channel: 0,
        down: true,
    });
    s.live_note(piano, 60, 0.0, false);
    assert!(
        !s.take_recorded_performance()
            .iter()
            .any(|e| matches!(e.kind, RecordedPerformanceKind::NoteOff { .. }))
    );
    s.handle_midi_event(MidiEvent::Sustain {
        channel: 0,
        down: false,
    });
    assert!(
        s.take_recorded_performance()
            .iter()
            .any(|e| e.voice == voice && matches!(e.kind, RecordedPerformanceKind::NoteOff { .. }))
    );
}

#[test]
fn mpe_rpn_disabled_zone_can_reenable_but_explicit_off_stops_listening() {
    let mut s = mpe_session(MpeMode::Upper, 4);
    for (cc, value) in [(101, 0), (100, 6), (6, 0)] {
        mpe_cc(&mut s, 15, cc, value);
    }
    assert_eq!(s.mpe_config().mode, MpeMode::Off);
    for (cc, value) in [(101, 0), (100, 6), (6, 5)] {
        mpe_cc(&mut s, 15, cc, value);
    }
    assert_eq!(s.mpe_config().mode, MpeMode::Upper);
    assert_eq!(s.mpe_config().members, 5);
    for (cc, value) in [(101, 0), (100, 6), (6, 0)] {
        mpe_cc(&mut s, 15, cc, value);
    }
    let off = s.mpe_config();
    s.configure_mpe(off).unwrap();
    for (cc, value) in [(101, 0), (100, 6), (6, 5)] {
        mpe_cc(&mut s, 15, cc, value);
    }
    assert_eq!(
        s.mpe_config().mode,
        MpeMode::Off,
        "manual Off also stops the private RPN listener"
    );
}
#[test]
fn mpe_lsb_only_range_change_keeps_coarse_and_cc74_midpoint_is_neutral() {
    let mut s = mpe_session(MpeMode::Lower, 4);
    for (cc, value) in [(101, 0), (100, 0), (38, 50)] {
        mpe_cc(&mut s, 1, cc, value);
    }
    assert_eq!(s.mpe_config().member_bend_range, 48.5);
    mpe_cc(&mut s, 1, 74, 64);
    mpe_cc(&mut s, 0, 74, 64);
    mpe_on(&mut s, 1, 60);
    let events = s.take_recorded_performance();
    let on = events
        .iter()
        .find(|e| matches!(e.kind, RecordedPerformanceKind::NoteOn { .. }))
        .unwrap();
    assert_eq!(mpe_expression(&events, on.voice), (0.0, 0.0, 0.5));
}

#[test]
fn mpe_wide_manager_range_reaches_internal_and_member_recording_without_global_clipping() {
    let mut s = mpe_session(MpeMode::Lower, 2);
    s.configure_mpe(MpeConfig {
        mode: MpeMode::Lower,
        members: 2,
        manager_bend_range: 96.0,
        ..Default::default()
    })
    .unwrap();
    let piano = s.live_target().unwrap();
    s.take_recorded_performance();
    s.live_note(piano, 0, 0.8, true);
    assert!(s.pitch_bend(1.0));
    mpe_on(&mut s, 1, 0);
    let events = s.take_recorded_performance();
    let voices: Vec<_> = events
        .iter()
        .filter(|e| matches!(e.kind, RecordedPerformanceKind::NoteOn { .. }))
        .map(|e| e.voice)
        .collect();
    assert_eq!(voices.len(), 2);
    assert_eq!(s.pitch_bend_range(), 96.0);
    for voice in &voices {
        assert_eq!(mpe_expression(&events, *voice).0, 96.0);
    }
    s.handle_midi_event(MidiEvent::PitchBend {
        channel: 0,
        value: -8192,
    });
    let events = s.take_recorded_performance();
    for voice in &voices {
        assert_eq!(mpe_expression(&events, *voice).0, -96.0);
    }
}

#[test]
fn mpe_lift_velocity_survives_raw_input_and_sustain_release() {
    for sustained in [false, true] {
        let mut s = mpe_session(MpeMode::Lower, 3);
        mpe_on(&mut s, 1, 60);
        s.take_recorded_performance();
        if sustained {
            s.handle_midi_event(MidiEvent::Sustain {
                channel: 0,
                down: true,
            });
        }
        s.handle_midi_event(MidiEvent::NoteOff {
            channel: 1,
            key: 60,
            velocity: 91,
        });
        if sustained {
            assert!(
                !s.take_recorded_performance()
                    .iter()
                    .any(|e| matches!(e.kind, RecordedPerformanceKind::NoteOff { .. }))
            );
            s.handle_midi_event(MidiEvent::Sustain {
                channel: 0,
                down: false,
            });
        }
        let events = s.take_recorded_performance();
        let release = events
            .iter()
            .find_map(|e| match e.kind {
                RecordedPerformanceKind::NoteOff { velocity, .. } => Some(velocity),
                _ => None,
            })
            .expect("release");
        assert!(
            (release - 91.0 / 127.0).abs() < 1e-7,
            "sustain={sustained}: {release}"
        );
    }
}
