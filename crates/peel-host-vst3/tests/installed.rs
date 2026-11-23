//! この PC に入っている VST3 で、読み込み・発音・状態の保存を確かめる。
//! プラグインがなければ何もしない。`PEEL_VST3_TEST` で別のファイルも試せる。

use std::path::PathBuf;

use peel_dsp::{Device, DeviceEvent, EventKind, ProcessContext};
use peel_host_vst3::{Vst3Device, scan_file};

fn plugin_file() -> Option<PathBuf> {
    let path = std::env::var_os("PEEL_VST3_TEST")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Program Files\Common Files\VST3\Vintage.vst3"));
    path.exists().then_some(path)
}

fn render(dev: &mut Vst3Device, blocks: usize, events: &[DeviceEvent]) -> f32 {
    let ctx = ProcessContext::new(48_000.0);
    let (mut l, mut r) = (vec![0.0f32; 256], vec![0.0f32; 256]);
    let mut peak = 0.0f32;
    for b in 0..blocks {
        l.fill(0.0);
        r.fill(0.0);
        dev.process(&ctx, if b == 0 { events } else { &[] }, &mut l, &mut r);
        peak = l.iter().chain(&r).fold(peak, |m, s| m.max(s.abs()));
        assert!(l.iter().chain(&r).all(|s| s.is_finite()));
    }
    peak
}

#[test]
fn an_installed_plugin_loads_plays_and_keeps_its_state() {
    let Some(path) = plugin_file() else {
        eprintln!("VST3 がないので省略");
        return;
    };
    let infos = scan_file(&path).expect("scan");
    let info = infos.first().expect("a class").clone();
    eprintln!("{} / {} / {:?}", info.name, info.vendor, info.role);
    let mut dev = Vst3Device::load(&info, 48_000.0, 256).expect("load");
    let handle = dev.handle();
    let desc = dev.descriptor();
    eprintln!("parameters: {}", desc.params.len());
    assert_eq!(desc.type_id, info.type_id());

    if info.is_instrument() {
        let on = DeviceEvent {
            offset: 0,
            kind: EventKind::NoteOn {
                key: 60,
                velocity: 0.9,
                voice: 1,
            },
        };
        let peak = render(&mut dev, 200, &[on]);
        eprintln!("peak while playing: {peak}");
        // 登録しないと音を出さない製品もあるので（Vintage がそう）、音の確認は
        // PEEL_VST3_EXPECT_SOUND を指定したときだけ。
        if std::env::var_os("PEEL_VST3_EXPECT_SOUND").is_some() {
            assert!(peak > 1e-4, "the instrument made no sound");
        }
        let off = DeviceEvent {
            offset: 0,
            kind: EventKind::NoteOff {
                key: 60,
                velocity: 0.0,
                voice: 1,
            },
        };
        render(&mut dev, 4, &[off]);
    }

    // パラメータを動かすと、状態を保存して戻した後も残る。
    if let Some(p) = desc.params.iter().find(|p| p.choices.is_empty()) {
        let before = handle.param(p.id).unwrap();
        let target = if before > 0.5 { 0.2 } else { 0.8 };
        dev.set_param(p.id, target);
        render(&mut dev, 2, &[]);
        assert!((dev.param(p.id) - target).abs() < 1e-6);
        let _ = handle.idle();
        let text = handle.param_text(p.id, target);
        eprintln!("{} = {:?}", p.name, text);
        let state = handle.save_state().expect("save");
        assert!(!state.is_empty());

        let mut fresh = Vst3Device::load(&info, 48_000.0, 256).expect("reload");
        fresh.load_state(&state).expect("restore");
        let restored = fresh.handle().param(p.id).unwrap();
        eprintln!("{}: {before} -> {target}, restored {restored}", p.name);
        assert!(
            (restored - target).abs() < 0.02,
            "the state did not keep {}",
            p.name
        );
    }
    drop(dev);
    let _ = handle.idle();
}
