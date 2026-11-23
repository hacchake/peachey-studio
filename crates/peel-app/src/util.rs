//! Small helpers shared by the editors.

use peel_document::{Color as DocColor, Time, TimeSignature};
use peel_ui::{Color, Id as W, Rect, Ui, Vec2, icons, vec2};

/// Thickness of the timeline scrollbars.
pub const SCROLLBAR: f32 = 8.0;

/// Names for the twelve palette colors, in palette order.
pub const COLOR_NAMES: [&str; 12] = [
    "Red", "Orange", "Yellow", "Lime", "Jade", "Teal", "Blue", "Violet", "Pink", "Coral", "Sand",
    "Slate",
];

/// Document color to UI color.
pub fn col(c: DocColor) -> Color {
    Color::from_rgb8(c.r, c.g, c.b)
}

/// Text color that reads on top of a fill.
pub fn ink_on(fill: Color) -> Color {
    if fill.luminance() > 0.38 {
        Color::hex(0x1A1410)
    } else {
        Color::hex(0xFBF7EF)
    }
}

/// "bar.beat.sixteenth" (1-based) for a position in quarter notes.
pub fn format_position(beat: f64, sig: TimeSignature) -> String {
    let bar_len = sig.bar_length().as_f64();
    let beat_len = sig.beat_length().as_f64();
    let beat = beat.max(0.0);
    let bar = (beat / bar_len).floor();
    let in_bar = beat - bar * bar_len;
    let b = (in_bar / beat_len).floor();
    let sub = ((in_bar - b * beat_len) / (beat_len / 4.0)).floor();
    format!("{}.{}.{}", bar as i64 + 1, b as i64 + 1, sub as i64 + 1)
}

/// Musical length label for a grid or note length.
pub fn format_length(t: Time) -> String {
    // Exact rational matches: triplets never pass through floats.
    let table: [(Time, &str); 16] = [
        (Time::new(4, 1), "1 bar"),
        (Time::new(2, 1), "1/2"),
        (Time::new(1, 1), "1/4"),
        (Time::new(1, 2), "1/8"),
        (Time::new(1, 4), "1/16"),
        (Time::new(1, 8), "1/32"),
        (Time::new(1, 16), "1/64"),
        (Time::new(2, 3), "1/4T"),
        (Time::new(1, 3), "1/8T"),
        (Time::new(1, 6), "1/16T"),
        (Time::new(1, 12), "1/32T"),
        (Time::new(3, 8), "1/16D"),
        (Time::new(3, 4), "1/8D"),
        (Time::new(3, 2), "1/4D"),
        (Time::new(8, 1), "2 bars"),
        (Time::new(16, 1), "4 bars"),
    ];
    for (b, l) in table {
        if t == b {
            return l.to_string();
        }
    }
    format!("{:.2}", t.as_f64())
}

/// Note name, using C4 = MIDI 60.
pub fn note_name(key: u8) -> String {
    const NAMES: [&str; 12] = [
        "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
    ];
    format!("{}{}", NAMES[(key % 12) as usize], key as i32 / 12 - 1)
}

pub fn is_black_key(key: u8) -> bool {
    matches!(key % 12, 1 | 3 | 6 | 8 | 10)
}

pub use peel_document::{db_to_fader, fader_to_db};

pub fn format_db(db: f32) -> String {
    if db <= -95.0 {
        "-inf".into()
    } else {
        format!("{db:+.1}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_law_round_trips() {
        for db in [-48.0f32, -12.0, -6.0, 0.0, 3.0, 6.0] {
            assert!((fader_to_db(db_to_fader(db)) - db).abs() < 0.05, "{db}");
        }
    }

    #[test]
    fn positions() {
        let sig = TimeSignature::default();
        assert_eq!(format_position(0.0, sig), "1.1.1");
        assert_eq!(format_position(4.25, sig), "2.1.2");
        assert_eq!(format_position(7.5, sig), "2.4.3");
        assert_eq!(note_name(60), "C4");
    }
}

/// Per-frame copy of `Prefs::show_help`, so editors that only see the
/// workspace can ask whether usage hints are on.
#[derive(Default)]
struct HelpOn(bool);

const HELP: W = W(0x4e1b_77a0_5c03);

/// Sets whether usage hints show this frame (called by the studio).
pub fn set_help(ui: &mut Ui, on: bool) {
    ui.state::<HelpOn>(HELP).0 = on;
}

/// Usage hints are on.
pub fn help(ui: &mut Ui) -> bool {
    ui.state::<HelpOn>(HELP).0
}

/// An optional usage hint. With hints on, the text wraps into `r` and the
/// height used is returned; with hints off only a small info mark shows
/// at the top left of `r`, carrying the text as its tooltip (and name).
/// Warnings never go through here: they always show.
pub fn hint(ui: &mut Ui, id: W, r: Rect, text: &str) -> f32 {
    let t = ui.theme.clone();
    if help(ui) {
        ui.text_wrapped(r.min(), r.w, text, t.caption, t.text_faint)
    } else {
        let mark = Rect {
            x: r.x,
            y: r.y,
            w: 18.0,
            h: 18.0,
        };
        let resp = ui.interact_node(
            id,
            mark,
            peel_ui::FocusKind::Activate,
            peel_ui::Semantics::new(peel_ui::Role::Button)
                .label("Hint")
                .description(text),
        );
        let c = if resp.hovered {
            t.text_dim
        } else {
            t.text_faint
        };
        ui.icon(mark, icons::INFO, 14.0, c);
        ui.tooltip(&resp, text);
        ui.focus_ring(id, mark, 9.0);
        18.0
    }
}

/// Peak of a mixer track since the previous frame. Telemetry is read
/// destructively, so every view of the same track (the mixer strip, the
/// top-bar meter) shares one read per frame.
pub fn track_peaks(ui: &mut Ui, s: &peel_session::Session, track: peel_document::Id) -> (f32, f32) {
    let now = ui.time();
    let st = ui.state::<(f64, (f32, f32))>(W::new(("track-peaks", track)));
    if st.0 != now {
        *st = (now, s.take_track_meter(track));
    }
    st.1
}

/// Gain reduction (dB, positive) of a dynamics device since the previous
/// frame, read once per frame however many views show it.
pub fn reduction_db(ui: &mut Ui, s: &peel_session::Session, device: peel_document::Id) -> f32 {
    let now = ui.time();
    let st = ui.state::<(f64, f32)>(W::new(("reduction-read", device)));
    if st.0 != now {
        *st = (now, s.take_reduction_db(device));
    }
    st.1
}

/// True (after a short notice) when the project already has as many mixer
/// tracks as the engine can play; adding another would be silently wrong.
pub fn mixer_full(s: &mut peel_session::Session) -> bool {
    let full = s.project().mixer.tracks.len() >= peel_engine::MAX_TRACKS;
    if full {
        s.notify(
            format!("The mixer is full ({} tracks)", peel_engine::MAX_TRACKS),
            true,
        );
    }
    full
}

/// Linear peak as dBFS text; `-inf` below -90.
pub fn peak_text(peak: f32) -> String {
    if peak <= 3.2e-5 {
        "-inf".into()
    } else {
        format!("{:+.1}", 20.0 * peak.log10())
    }
}

/// Pan as the console legend: `C`, `L35`, `R100`.
pub fn format_pan(pan: f32) -> String {
    let v = (pan.abs() * 100.0).round() as i32;
    match v {
        0 => "C".into(),
        _ if pan < 0.0 => format!("L{v}"),
        _ => format!("R{v}"),
    }
}

#[cfg(test)]
mod audio_tests {
    use super::*;

    #[test]
    fn pan_and_peak_legends() {
        assert_eq!(format_pan(0.0), "C");
        assert_eq!(format_pan(-0.35), "L35");
        assert_eq!(format_pan(1.0), "R100");
        assert_eq!(peak_text(0.0), "-inf");
        assert_eq!(peak_text(1.0), "+0.0");
        assert_eq!(peak_text(0.5), "-6.0");
    }
}

/// The mark: a peach with its stem and two leaves, matching the app icon.
/// `radius` is roughly the fruit's.
pub fn logo(ui: &mut Ui, c: Vec2, radius: f32) {
    let k = radius / 12.0;
    let p = |x: f32, y: f32| c + vec2(x, y) * k;
    // 果肉: 左右のふくらみを少しずらした二つの円と、下側の黄色み。
    ui.scene.circle(p(-1.2, 1.2), 9.6 * k, Color::hex(0xF68A86));
    ui.scene.circle(p(1.4, 0.8), 10.2 * k, Color::hex(0xF0607A));
    ui.scene
        .circle(p(0.4, 3.8), 7.2 * k, Color::hex(0xF8A65E).alpha(0.3));
    ui.scene
        .circle(p(0.6, 5.2), 5.0 * k, Color::hex(0xF9B868).alpha(0.4));
    ui.scene
        .circle(p(3.6, -3.4), 1.8 * k, Color::hex(0xFFFFFF).alpha(0.3));
    // 真ん中の割れ目。
    let cleft: Vec<Vec2> = (0..=8)
        .map(|i| {
            let t = i as f32 / 8.0;
            let (a, m, b) = ((-0.6, -8.6), (-5.6, -0.6), (-3.0, 9.4));
            let x = (1.0 - t) * (1.0 - t) * a.0 + 2.0 * (1.0 - t) * t * m.0 + t * t * b.0;
            let y = (1.0 - t) * (1.0 - t) * a.1 + 2.0 * (1.0 - t) * t * m.1 + t * t * b.1;
            p(x, y)
        })
        .collect();
    ui.scene
        .polyline(&cleft, 1.1 * k, Color::hex(0xC8455E).alpha(0.7));
    // 軸と葉。
    ui.scene
        .line(p(0.2, -8.8), p(2.0, -12.6), 1.5 * k, Color::hex(0x7A4A28));
    ui.scene
        .line(p(-1.0, -9.6), p(-7.0, -12.0), 3.2 * k, Color::hex(0x2F7F33));
    ui.scene
        .line(p(2.6, -10.2), p(9.6, -11.2), 3.6 * k, Color::hex(0x43A047));
}
