//! VST3 プラグインの [`DeviceDescriptor`]。
//!
//! `Device` は `&'static DeviceDescriptor` を求めるが、パラメータはプラグインを
//! 作ってみるまで分からない。作るたびに漏らさないよう、同じ並びのものは
//! 使い回す（CLAP と同じやり方）。

use std::sync::{Mutex, PoisonError};

use peel_dsp::{DeviceDescriptor, DeviceKind, ParamDescriptor, Taper, Unit};

/// 段のある値がこの数以下なら選択肢として見せる。
pub(crate) const MAX_CHOICES: i32 = 64;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ParamSpec {
    pub(crate) id: u32,
    pub(crate) name: String,
    pub(crate) group: String,
    /// 0 なら連続値（0..1 の正規化値をそのまま使う）。
    pub(crate) steps: i32,
    pub(crate) default_normalized: f64,
    pub(crate) automatable: bool,
    pub(crate) choices: Vec<String>,
}

impl ParamSpec {
    pub(crate) fn plain(&self, normalized: f64) -> f32 {
        if self.choices.is_empty() {
            normalized.clamp(0.0, 1.0) as f32
        } else {
            (normalized.clamp(0.0, 1.0) * self.steps as f64).round() as f32
        }
    }

    pub(crate) fn normalized(&self, plain: f32) -> f64 {
        if self.choices.is_empty() {
            (plain as f64).clamp(0.0, 1.0)
        } else {
            (plain as f64 / self.steps.max(1) as f64).clamp(0.0, 1.0)
        }
    }

    fn max(&self) -> f32 {
        if self.choices.is_empty() {
            1.0
        } else {
            self.steps as f32
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DescriptorSpec {
    pub(crate) type_id: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) kind: DeviceKind,
    pub(crate) params: Vec<ParamSpec>,
}

impl DescriptorSpec {
    fn matches(&self, d: &DeviceDescriptor) -> bool {
        self.type_id == d.type_id
            && self.name == d.name
            && self.kind == d.kind
            && self.params.len() == d.params.len()
            && self.params.iter().zip(d.params).all(|(s, p)| {
                s.id == p.id
                    && s.name == p.name
                    && s.group == p.group
                    && s.max() == p.max
                    && s.plain(s.default_normalized) == p.default
                    && s.automatable == p.automatable
                    && s.choices
                        .iter()
                        .map(String::as_str)
                        .eq(p.choices.iter().copied())
            })
    }

    fn leak(self) -> &'static DeviceDescriptor {
        fn leak_str(s: String) -> &'static str {
            Box::leak(s.into_boxed_str())
        }
        let params: Vec<ParamDescriptor> = self
            .params
            .iter()
            .map(|p| {
                let choices: Vec<&'static str> = p.choices.iter().cloned().map(leak_str).collect();
                ParamDescriptor {
                    id: p.id,
                    // キーはプラグイン自身の ID（10 進）。表示名と違い版が変わっても続く。
                    key: leak_str(p.id.to_string()),
                    name: leak_str(p.name.clone()),
                    group: leak_str(p.group.clone()),
                    min: 0.0,
                    max: p.max(),
                    default: p.plain(p.default_normalized),
                    unit: Unit::None,
                    taper: Taper::Linear,
                    choices: Box::leak(choices.into_boxed_slice()),
                    automatable: p.automatable,
                }
            })
            .collect();
        Box::leak(Box::new(DeviceDescriptor {
            type_id: leak_str(self.type_id),
            name: leak_str(self.name),
            description: leak_str(self.description),
            category: "VST3",
            kind: self.kind,
            version: 1,
            params: Box::leak(params.into_boxed_slice()),
        }))
    }
}

static CACHE: Mutex<Vec<&'static DeviceDescriptor>> = Mutex::new(Vec::new());

/// 一度読み込んだプラグインの記述子（`vst3:<クラス ID>`）。
pub fn cached_descriptor(type_id: &str) -> Option<&'static DeviceDescriptor> {
    let cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.iter().rev().find(|d| d.type_id == type_id).copied()
}

/// `spec` と同じ記述子があればそれを、なければ新しく作って返す。
pub(crate) fn intern(spec: DescriptorSpec) -> &'static DeviceDescriptor {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(found) = cache.iter().find(|d| spec.matches(d)) {
        return found;
    }
    let d = spec.leak();
    cache.push(d);
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(steps: i32, choices: usize) -> ParamSpec {
        ParamSpec {
            id: 7,
            name: "Cutoff".into(),
            group: String::new(),
            steps,
            default_normalized: 0.5,
            automatable: true,
            choices: (0..choices).map(|i| i.to_string()).collect(),
        }
    }

    #[test]
    fn continuous_values_pass_through_and_lists_use_indices() {
        let c = spec(0, 0);
        assert_eq!(c.plain(0.25), 0.25);
        assert_eq!(c.normalized(0.25), 0.25);
        let l = spec(3, 4);
        assert_eq!(l.plain(2.0 / 3.0), 2.0);
        assert!((l.normalized(1.0) - 1.0 / 3.0).abs() < 1e-9);
    }

    #[test]
    fn the_same_layout_reuses_one_descriptor() {
        let make = || DescriptorSpec {
            type_id: "vst3:TEST".into(),
            name: "Test".into(),
            description: String::new(),
            kind: DeviceKind::Effect,
            params: vec![spec(0, 0)],
        };
        let a = intern(make());
        let b = intern(make());
        assert!(std::ptr::eq(a, b));
        assert!(std::ptr::eq(cached_descriptor("vst3:TEST").unwrap(), a));
    }
}
