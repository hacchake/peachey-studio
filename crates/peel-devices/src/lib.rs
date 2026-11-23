//! Registry of every native device type.

use peel_dsp::{Device, DeviceDescriptor, DeviceKind, Preset};

/// All native device descriptors: instruments first, then effects.
pub fn all() -> impl Iterator<Item = &'static DeviceDescriptor> {
    peel_instruments::descriptors()
        .iter()
        .chain(peel_effects::descriptors().iter())
        .copied()
}

pub fn instruments() -> impl Iterator<Item = &'static DeviceDescriptor> {
    all().filter(|d| d.kind == DeviceKind::Instrument)
}

pub fn effects() -> impl Iterator<Item = &'static DeviceDescriptor> {
    all().filter(|d| d.kind == DeviceKind::Effect)
}

/// Descriptor for a native device type, or for a CLAP (`clap:<id>`) or
/// VST3 (`vst3:<class id>`) plugin once an instance of it has been loaded.
pub fn descriptor(type_id: &str) -> Option<&'static DeviceDescriptor> {
    all().find(|d| d.type_id == type_id).or_else(|| {
        if type_id.starts_with("clap:") {
            peel_host_clap::cached_descriptor(type_id)
        } else if type_id.starts_with("vst3:") {
            peel_host_vst3::cached_descriptor(type_id)
        } else {
            None
        }
    })
}

/// Creates a device with descriptor defaults applied. Call `prepare` on a
/// control thread before installing it in the engine.
pub fn create(type_id: &str) -> Option<Box<dyn Device>> {
    let mut device = peel_instruments::create(type_id).or_else(|| peel_effects::create(type_id))?;
    peel_dsp::device::apply_defaults(device.as_mut());
    Some(device)
}

/// Factory presets for a device type.
pub fn presets(type_id: &str) -> &'static [Preset] {
    let p = peel_instruments::presets(type_id);
    if p.is_empty() {
        peel_effects::presets(type_id)
    } else {
        p
    }
}

/// Applies a preset's values to a device (control thread).
pub fn apply_preset(device: &mut dyn Device, preset: &Preset) {
    let desc = device.descriptor();
    for (key, value) in preset.params {
        if let Some(p) = desc.param_by_key(key) {
            device.set_param(p.id, *value);
        }
    }
}

/// Device types with a sidechain (key) input that can listen to another
/// mixer track.
pub fn accepts_sidechain(type_id: &str) -> bool {
    matches!(type_id, "peel.fx.compressor" | "peel.fx.gate")
}

/// Device types that report gain reduction for a meter.
pub fn shows_reduction(type_id: &str) -> bool {
    matches!(
        type_id,
        "peel.fx.compressor" | "peel.fx.gate" | "peel.fx.limiter" | "peel.fx.mastering"
    )
}
