//! 走査で分かるプラグインの情報。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 楽器か、エフェクトか。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Vst3Role {
    Instrument,
    AudioEffect,
    Other,
}

/// VST3 ファイルの中の、音を扱うクラス 1 つ分。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Vst3PluginInfo {
    /// クラス ID（32 桁の 16 進）。プロジェクトに保存される型名の元。
    pub cid: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    /// `Fx|Delay` や `Instrument|Synth` のような分類。
    pub sub_categories: String,
    pub role: Vst3Role,
    /// `.vst3` ファイル（またはバンドルのフォルダ）。
    pub path: PathBuf,
}

impl Vst3PluginInfo {
    /// デバイスの型名（`vst3:<クラス ID>`）。
    pub fn type_id(&self) -> String {
        format!("vst3:{}", self.cid)
    }

    pub fn is_instrument(&self) -> bool {
        self.role == Vst3Role::Instrument
    }

    pub fn is_effect(&self) -> bool {
        self.role == Vst3Role::AudioEffect
    }

    pub fn is_hostable(&self) -> bool {
        matches!(self.role, Vst3Role::Instrument | Vst3Role::AudioEffect)
    }
}

/// サブカテゴリから役割を決める。分類のないものはエフェクトとみなす。
pub(crate) fn classify(sub_categories: &str) -> Vst3Role {
    let parts: Vec<&str> = sub_categories.split('|').map(str::trim).collect();
    if parts.iter().any(|p| p.eq_ignore_ascii_case("Instrument")) {
        Vst3Role::Instrument
    } else if parts
        .iter()
        .all(|p| p.eq_ignore_ascii_case("Analyzer") || p.is_empty())
        && !sub_categories.trim().is_empty()
    {
        Vst3Role::Other
    } else {
        Vst3Role::AudioEffect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_categories_decide_the_role() {
        assert_eq!(classify("Instrument|Synth"), Vst3Role::Instrument);
        assert_eq!(classify("Fx|Delay"), Vst3Role::AudioEffect);
        assert_eq!(classify(""), Vst3Role::AudioEffect);
        assert_eq!(classify("Analyzer"), Vst3Role::Other);
        assert_eq!(classify("Fx|Analyzer"), Vst3Role::AudioEffect);
    }
}
