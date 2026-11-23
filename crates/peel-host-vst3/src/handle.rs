//! メインスレッドから VST3 プラグインを操作する口。

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError};

use peel_dsp::{DeviceDescriptor, ParamId};
use vst3::Steinberg::Vst::RestartFlags_::*;

use crate::editor::Editor;
use crate::info::Vst3PluginInfo;
use crate::plugin::Plugin;

/// [`Vst3Handle::idle`] の結果。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IdleReport {
    /// 入出力や遅延が変わったので、作り直してエンジンに入れ直してほしい。
    pub restart_requested: bool,
}

/// メインスレッドで使う操作口。複製しても同じプラグインを指す。
#[derive(Clone)]
pub struct Vst3Handle {
    plugin: Arc<Plugin>,
}

impl Vst3Handle {
    pub(crate) fn new(plugin: Arc<Plugin>) -> Vst3Handle {
        Vst3Handle { plugin }
    }

    pub fn info(&self) -> &Vst3PluginInfo {
        &self.plugin.info
    }

    pub fn descriptor(&self) -> &'static DeviceDescriptor {
        self.plugin.descriptor
    }

    /// 毎フレーム呼ぶ。ホスト側の値を画面に伝え、閉じた画面を片付ける。
    pub fn idle(&self) -> IdleReport {
        self.plugin.push_host_values();
        let flags = self.plugin.shared.restart_flags.swap(0, Ordering::Relaxed);
        if flags & kParamValuesChanged != 0 {
            self.plugin.refresh_values();
        }
        let mut editor = self
            .plugin
            .editor
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if editor.as_ref().is_some_and(Editor::is_closed)
            && let Some(e) = editor.take()
        {
            e.close();
        }
        IdleReport {
            restart_requested: flags & (kReloadComponent | kIoChanged | kLatencyChanged) != 0,
        }
    }

    pub fn save_state(&self) -> Result<Vec<u8>, String> {
        self.plugin.save_state()
    }

    pub fn load_state(&self, data: &[u8]) -> Result<(), String> {
        self.plugin.load_state(data)
    }

    /// パラメータの今の値（記述子の範囲の値）。
    pub fn param(&self, id: ParamId) -> Option<f32> {
        let shared = &self.plugin.shared;
        let &i = shared.index_of.get(&id)?;
        Some(shared.specs[i].plain(shared.get(i)))
    }

    /// プラグインの画面で最後に変えられた値。ホスト側で変えた後なら `None`。
    pub fn edited_in_window(&self, id: ParamId) -> Option<f32> {
        let shared = &self.plugin.shared;
        let &i = shared.index_of.get(&id)?;
        shared.gui_edited[i]
            .load(Ordering::Relaxed)
            .then(|| shared.specs[i].plain(shared.get(i)))
    }

    /// プラグイン自身の表示文字列。
    pub fn param_text(&self, id: ParamId, plain: f32) -> Option<String> {
        self.plugin.param_text(id, plain)
    }

    pub fn latency(&self) -> u32 {
        self.plugin.shared.latency.load(Ordering::Relaxed)
    }

    /// 画面を持っていそうか（実際に開けるかは開いてみるまで分からない）。
    pub fn has_editor(&self) -> bool {
        self.plugin.controller.is_some() && cfg!(windows)
    }

    pub fn editor_open(&self) -> bool {
        self.plugin
            .editor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .is_some_and(|e| !e.is_closed())
    }

    /// 画面を開く。開いていれば手前に出す。
    pub fn open_editor(&self) -> Result<(), String> {
        let mut editor = self
            .plugin
            .editor
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(e) = editor.as_ref() {
            if !e.is_closed() {
                e.raise();
                return Ok(());
            }
            if let Some(e) = editor.take() {
                e.close();
            }
        }
        *editor = Some(Editor::open(&self.plugin)?);
        Ok(())
    }

    pub fn close_editor(&self) {
        if let Some(e) = self
            .plugin
            .editor
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            e.close();
        }
    }
}
