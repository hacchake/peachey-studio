//! 読み込んだ VST3 プラグイン 1 つ分。音の処理側（コンポーネント）と
//! 画面側（コントローラ）を持ち、[`crate::Vst3Device`] と
//! [`crate::Vst3Handle`] が共有する。
//!
//! コントローラはメインスレッドだけで触る。処理側の `process` は音声
//! スレッドで呼ぶ。両者の間のパラメータ値は [`Shared`] の原子変数と
//! キューでやりとりする。

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use peel_dsp::{DeviceDescriptor, DeviceKind};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper};

use crate::com::{ComponentHandler, EditSink, HostApp, MemoryStream, from_wide};
use crate::descriptor::{DescriptorSpec, MAX_CHOICES, ParamSpec, intern};
use crate::info::Vst3PluginInfo;
use crate::module::{Module, cid_from_hex, open};

/// 画面から音声スレッドへ渡すパラメータ変化の上限（1 ブロックあたり）。
const GUI_QUEUE: usize = 1024;

/// 音声スレッドとメインスレッドが共有する値。
pub(crate) struct Shared {
    pub(crate) specs: Vec<ParamSpec>,
    pub(crate) index_of: HashMap<u32, usize>,
    /// 各パラメータの正規化値（f64 のビット列）。
    pub(crate) values: Vec<AtomicU64>,
    /// ホスト側（自動化や画面のつまみ）で変わり、コントローラに伝える必要がある。
    pub(crate) host_dirty: Vec<AtomicBool>,
    /// 最後に値を変えたのがプラグインの画面だった。
    pub(crate) gui_edited: Vec<AtomicBool>,
    /// プラグインの画面で変わり、処理側に伝える必要がある値。
    pub(crate) gui_tx: Mutex<rtrb::Producer<(u32, f64)>>,
    pub(crate) restart_flags: AtomicI32,
    pub(crate) latency: AtomicU32,
    pub(crate) offline: AtomicBool,
}

impl Shared {
    pub(crate) fn get(&self, index: usize) -> f64 {
        f64::from_bits(self.values[index].load(Ordering::Relaxed))
    }

    pub(crate) fn set(&self, index: usize, normalized: f64) {
        self.values[index].store(normalized.to_bits(), Ordering::Relaxed);
    }
}

impl EditSink for Shared {
    fn perform_edit(&self, id: ParamID, normalized: f64) {
        if let Some(&i) = self.index_of.get(&id) {
            self.set(i, normalized);
            self.gui_edited[i].store(true, Ordering::Relaxed);
        }
        // 表に出していないパラメータでも処理側には伝える。
        let mut tx = self.gui_tx.lock().unwrap_or_else(PoisonError::into_inner);
        let _ = tx.push((id, normalized));
    }

    fn restart(&self, flags: i32) {
        self.restart_flags.fetch_or(flags, Ordering::Relaxed);
    }
}

pub(crate) struct Plugin {
    pub(crate) info: Vst3PluginInfo,
    pub(crate) descriptor: &'static DeviceDescriptor,
    pub(crate) component: ComPtr<IComponent>,
    pub(crate) processor: ComPtr<IAudioProcessor>,
    pub(crate) controller: Option<ComPtr<IEditController>>,
    separate_controller: bool,
    connections: Option<(ComPtr<IConnectionPoint>, ComPtr<IConnectionPoint>)>,
    _host: ComWrapper<HostApp>,
    _handler: ComWrapper<ComponentHandler>,
    _module: Arc<Module>,
    pub(crate) shared: Arc<Shared>,
    pub(crate) pitch_bend: Option<ParamID>,
    pub(crate) aftertouch: Option<ParamID>,
    pub(crate) editor: Mutex<Option<crate::editor::Editor>>,
}

// SAFETY: コンポーネントとプロセッサは VST3 の約束どおりのスレッドで呼ぶ
// （準備はメインスレッド、process は音声スレッド）。コントローラと画面は
// メインスレッドでしか触らない。
unsafe impl Send for Plugin {}
unsafe impl Sync for Plugin {}

fn ok(r: tresult) -> bool {
    r == kResultOk || r == kResultTrue
}

impl Plugin {
    /// プラグインを作って初期化し、コントローラとつなぐ。メインスレッドで呼ぶ。
    #[allow(clippy::type_complexity)]
    pub(crate) fn load(
        info: &Vst3PluginInfo,
    ) -> Result<(Arc<Plugin>, rtrb::Consumer<(u32, f64)>), String> {
        let module = open(&info.path)?;
        let cid = cid_from_hex(&info.cid)
            .ok_or_else(|| format!("{} のクラス ID が読めません", info.name))?;
        let component: ComPtr<IComponent> = module
            .create(&cid)
            .ok_or_else(|| format!("{} を作れませんでした", info.name))?;
        let host = ComWrapper::new(HostApp);
        let context = host
            .as_com_ref::<IHostApplication>()
            .map(|r| r.as_ptr() as *mut FUnknown)
            .ok_or("ホストの情報を用意できません")?;
        unsafe {
            if !ok(component.initialize(context)) {
                return Err(format!("{} の初期化に失敗しました", info.name));
            }
        }
        let processor = component
            .cast::<IAudioProcessor>()
            .ok_or_else(|| format!("{} は音を処理できません", info.name))?;

        // 画面側: 同じオブジェクトか、別クラスか。
        let (controller, separate) = match component.cast::<IEditController>() {
            Some(c) => (Some(c), false),
            None => unsafe {
                let mut tuid: TUID = [0; 16];
                if ok(component.getControllerClassId(&mut tuid)) {
                    let cid = tuid.map(|b| b as u8);
                    match module.create::<IEditController>(&cid) {
                        Some(c) if ok(c.initialize(context)) => (Some(c), true),
                        _ => (None, false),
                    }
                } else {
                    (None, false)
                }
            },
        };

        // 処理側と画面側を互いにつなぐ。
        let connections = match (&controller, separate) {
            (Some(c), true) => match (
                component.cast::<IConnectionPoint>(),
                c.cast::<IConnectionPoint>(),
            ) {
                (Some(a), Some(b)) => unsafe {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                    Some((a, b))
                },
                _ => None,
            },
            _ => None,
        };

        // 処理側の状態を画面側に知らせる。
        if let Some(c) = &controller {
            let stream = ComWrapper::new(MemoryStream::default());
            if let Some(s) = stream.as_com_ref::<IBStream>() {
                unsafe {
                    if ok(component.getState(s.as_ptr())) {
                        let mut pos = 0;
                        s.seek(0, 0, &mut pos);
                        c.setComponentState(s.as_ptr());
                    }
                }
            }
        }

        let specs = controller.as_ref().map(read_params).unwrap_or_default();
        let (pitch_bend, aftertouch) = controller
            .as_ref()
            .and_then(|c| c.cast::<IMidiMapping>())
            .map(|m| unsafe {
                let mut pb = 0;
                let mut at = 0;
                let pb = ok(m.getMidiControllerAssignment(
                    0,
                    0,
                    ControllerNumbers_::kPitchBend as i16,
                    &mut pb,
                ))
                .then_some(pb);
                let at = ok(m.getMidiControllerAssignment(
                    0,
                    0,
                    ControllerNumbers_::kAfterTouch as i16,
                    &mut at,
                ))
                .then_some(at);
                (pb, at)
            })
            .unwrap_or((None, None));

        let kind = if info.is_instrument() {
            DeviceKind::Instrument
        } else {
            DeviceKind::Effect
        };
        let descriptor = intern(DescriptorSpec {
            type_id: info.type_id(),
            name: info.name.clone(),
            description: format!("{} — VST3", info.vendor),
            kind,
            params: specs.clone(),
        });
        let index_of = specs.iter().enumerate().map(|(i, p)| (p.id, i)).collect();
        let values = specs
            .iter()
            .map(|p| {
                let v = controller
                    .as_ref()
                    .map_or(p.default_normalized, |c| unsafe {
                        c.getParamNormalized(p.id)
                    });
                AtomicU64::new(v.to_bits())
            })
            .collect();
        let host_dirty = specs.iter().map(|_| AtomicBool::new(false)).collect();
        let gui_edited = specs.iter().map(|_| AtomicBool::new(false)).collect();
        let (gui_tx, gui_rx) = rtrb::RingBuffer::new(GUI_QUEUE);
        let shared = Arc::new(Shared {
            specs,
            index_of,
            values,
            host_dirty,
            gui_edited,
            gui_tx: Mutex::new(gui_tx),
            restart_flags: AtomicI32::new(0),
            latency: AtomicU32::new(0),
            offline: AtomicBool::new(false),
        });

        let handler = ComWrapper::new(ComponentHandler {
            sink: shared.clone(),
        });
        if let (Some(c), Some(h)) = (&controller, handler.as_com_ref::<IComponentHandler>()) {
            unsafe { c.setComponentHandler(h.as_ptr()) };
        }

        let plugin = Arc::new(Plugin {
            info: info.clone(),
            descriptor,
            component,
            processor,
            controller,
            separate_controller: separate,
            connections,
            _host: host,
            _handler: handler,
            _module: module,
            shared,
            pitch_bend,
            aftertouch,
            editor: Mutex::new(None),
        });
        Ok((plugin, gui_rx))
    }

    /// 処理側と画面側の状態を 1 つの塊にする。
    pub(crate) fn save_state(&self) -> Result<Vec<u8>, String> {
        let read = |f: &dyn Fn(*mut IBStream) -> tresult| -> Option<Vec<u8>> {
            let stream = ComWrapper::new(MemoryStream::default());
            let s = stream.as_com_ref::<IBStream>()?;
            ok(f(s.as_ptr())).then(|| stream.take())
        };
        let component = read(&|s| unsafe { self.component.getState(s) })
            .ok_or_else(|| format!("{} の状態を読めません", self.info.name))?;
        let controller = self
            .controller
            .as_ref()
            .filter(|_| self.separate_controller)
            .and_then(|c| read(&|s| unsafe { c.getState(s) }))
            .unwrap_or_default();
        Ok(encode_state(&component, &controller))
    }

    /// [`Plugin::save_state`] で作った塊を戻す。
    pub(crate) fn load_state(&self, data: &[u8]) -> Result<(), String> {
        let (component, controller) = decode_state(data).ok_or("VST3 の状態の形式が違います")?;
        let stream = |bytes: &[u8]| ComWrapper::new(MemoryStream::with_data(bytes));
        unsafe {
            let s = stream(component);
            let p = s.as_com_ref::<IBStream>().ok_or("状態を渡せません")?;
            if !ok(self.component.setState(p.as_ptr())) {
                return Err(format!("{} が状態を受け取りませんでした", self.info.name));
            }
            if let Some(c) = &self.controller {
                let s = stream(component);
                if let Some(p) = s.as_com_ref::<IBStream>() {
                    c.setComponentState(p.as_ptr());
                }
                if self.separate_controller && !controller.is_empty() {
                    let s = stream(controller);
                    if let Some(p) = s.as_com_ref::<IBStream>() {
                        c.setState(p.as_ptr());
                    }
                }
            }
        }
        self.refresh_values();
        Ok(())
    }

    /// コントローラから今の値を読み直す。メインスレッド。
    pub(crate) fn refresh_values(&self) {
        let Some(c) = &self.controller else { return };
        for (i, p) in self.shared.specs.iter().enumerate() {
            self.shared.set(i, unsafe { c.getParamNormalized(p.id) });
        }
    }

    /// ホスト側で変わった値をコントローラ（画面）に伝える。メインスレッド。
    pub(crate) fn push_host_values(&self) {
        let Some(c) = &self.controller else { return };
        for (i, p) in self.shared.specs.iter().enumerate() {
            if self.shared.host_dirty[i].swap(false, Ordering::Relaxed) {
                unsafe { c.setParamNormalized(p.id, self.shared.get(i)) };
            }
        }
    }

    pub(crate) fn param_text(&self, id: u32, plain: f32) -> Option<String> {
        let c = self.controller.as_ref()?;
        let spec = &self.shared.specs[*self.shared.index_of.get(&id)?];
        let mut s: String128 = [0; 128];
        unsafe { ok(c.getParamStringByValue(id, spec.normalized(plain), &mut s)) }
            .then(|| from_wide(&s))
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        unsafe {
            if let Some((a, b)) = &self.connections {
                a.disconnect(b.as_ptr());
                b.disconnect(a.as_ptr());
            }
            if let Some(c) = &self.controller {
                c.setComponentHandler(std::ptr::null_mut());
                if self.separate_controller {
                    c.terminate();
                }
            }
            self.component.terminate();
        }
    }
}

fn read_params(c: &ComPtr<IEditController>) -> Vec<ParamSpec> {
    use ParameterInfo_::ParameterFlags_::*;
    let mut out = Vec::new();
    unsafe {
        let n = c.getParameterCount();
        for i in 0..n.max(0) {
            let mut pi: ParameterInfo = std::mem::zeroed();
            if !ok(c.getParameterInfo(i, &mut pi)) {
                continue;
            }
            let flags = pi.flags;
            // 隠し・読み取り専用・バイパスは、ホストのつまみには出さない。
            if flags & (kIsHidden | kIsReadOnly | kIsBypass) != 0 {
                continue;
            }
            let steps = pi.stepCount.max(0);
            let list =
                steps > 0 && (flags & kIsList != 0 || steps <= MAX_CHOICES) && steps <= MAX_CHOICES;
            let choices = if list {
                (0..=steps)
                    .map(|k| {
                        let mut s: String128 = [0; 128];
                        let v = k as f64 / steps as f64;
                        if ok(c.getParamStringByValue(pi.id, v, &mut s)) {
                            from_wide(&s)
                        } else {
                            k.to_string()
                        }
                    })
                    .collect()
            } else {
                Vec::new()
            };
            let title = from_wide(&pi.title);
            out.push(ParamSpec {
                id: pi.id,
                name: if title.is_empty() {
                    format!("#{}", pi.id)
                } else {
                    title
                },
                group: String::new(),
                steps: if list { steps } else { 0 },
                default_normalized: pi.defaultNormalizedValue,
                automatable: flags & kCanAutomate != 0,
                choices,
            });
        }
    }
    out
}

const STATE_MAGIC: &[u8; 4] = b"PV3S";

fn encode_state(component: &[u8], controller: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(12 + component.len() + controller.len());
    out.extend_from_slice(STATE_MAGIC);
    out.extend_from_slice(&(component.len() as u32).to_le_bytes());
    out.extend_from_slice(component);
    out.extend_from_slice(&(controller.len() as u32).to_le_bytes());
    out.extend_from_slice(controller);
    out
}

fn decode_state(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let rest = data.strip_prefix(STATE_MAGIC)?;
    let (n, rest) = rest.split_at_checked(4)?;
    let n = u32::from_le_bytes(n.try_into().ok()?) as usize;
    let (component, rest) = rest.split_at_checked(n)?;
    let (m, rest) = rest.split_at_checked(4)?;
    let m = u32::from_le_bytes(m.try_into().ok()?) as usize;
    let controller = rest.get(..m)?;
    Some((component, controller))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_blobs_round_trip_and_reject_garbage() {
        let blob = encode_state(b"abc", b"de");
        assert_eq!(decode_state(&blob), Some((&b"abc"[..], &b"de"[..])));
        assert_eq!(
            decode_state(&encode_state(b"", b"")),
            Some((&b""[..], &b""[..]))
        );
        assert_eq!(decode_state(b"nope"), None);
        assert_eq!(decode_state(&blob[..6]), None);
    }
}
