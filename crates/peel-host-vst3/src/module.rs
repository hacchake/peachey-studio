//! `.vst3` ファイルを読み込み、中のクラスを列挙して作る。
//!
//! 一度読み込んだモジュールはアプリが終わるまで解放しない。読み込みと
//! 解放を繰り返すと落ちるプラグインがあるため。

use std::collections::HashMap;
use std::ffi::c_char;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use vst3::Steinberg::*;
use vst3::{ComPtr, Interface};

use crate::com::from_c;

/// 音を扱うクラスの分類名。
const AUDIO_MODULE_CLASS: &str = "Audio Module Class";

pub(crate) struct Module {
    pub(crate) factory: ComPtr<IPluginFactory>,
    _lib: libloading::Library,
}

// SAFETY: ファクトリは VST3 の約束でどのスレッドから呼んでもよい。
unsafe impl Send for Module {}
unsafe impl Sync for Module {}

/// ファイルの中の 1 クラス。
#[derive(Clone, Debug)]
pub(crate) struct ClassEntry {
    pub(crate) cid: [u8; 16],
    pub(crate) name: String,
    pub(crate) category: String,
    pub(crate) sub_categories: String,
    pub(crate) vendor: String,
    pub(crate) version: String,
}

impl ClassEntry {
    pub(crate) fn is_audio_module(&self) -> bool {
        self.category == AUDIO_MODULE_CLASS
    }
}

pub(crate) fn cid_hex(cid: &[u8; 16]) -> String {
    cid.iter().map(|b| format!("{b:02X}")).collect()
}

pub(crate) fn cid_from_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// バンドルなら中の実体の DLL を、単体ファイルならそのまま返す。
fn binary_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_file() {
        return Ok(path.to_path_buf());
    }
    if !path.is_dir() {
        return Err(format!("{} が見つかりません", path.display()));
    }
    let name = path
        .file_name()
        .ok_or_else(|| format!("{} は読めない名前です", path.display()))?;
    let arch: &[&str] = if cfg!(target_arch = "aarch64") {
        &["arm64-win", "arm64ec-win", "arm64x-win"]
    } else {
        &["x86_64-win"]
    };
    arch.iter()
        .map(|a| path.join("Contents").join(a).join(name))
        .find(|p| p.is_file())
        .ok_or_else(|| format!("{} の中にこの PC 用の本体がありません", path.display()))
}

fn modules() -> &'static Mutex<HashMap<PathBuf, Arc<Module>>> {
    static MODULES: OnceLock<Mutex<HashMap<PathBuf, Arc<Module>>>> = OnceLock::new();
    MODULES.get_or_init(Default::default)
}

/// `.vst3` を読み込む（読み込み済みなら同じものを返す）。
pub(crate) fn open(path: &Path) -> Result<Arc<Module>, String> {
    if !cfg!(windows) {
        return Err("VST3 の読み込みは今のところ Windows だけに対応しています".into());
    }
    let mut map = modules().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(m) = map.get(path) {
        return Ok(m.clone());
    }
    let binary = binary_path(path)?;
    // SAFETY: プラグインの DLL を読み込む。初期化処理はプラグイン次第。
    let lib = unsafe { libloading::Library::new(&binary) }
        .map_err(|e| format!("{} を読み込めません: {e}", binary.display()))?;
    unsafe {
        if let Ok(init) = lib.get::<unsafe extern "system" fn() -> bool>(b"InitDll\0")
            && !init()
        {
            return Err(format!(
                "{} の初期化（InitDll）に失敗しました",
                binary.display()
            ));
        }
    }
    let get_factory = unsafe {
        lib.get::<unsafe extern "system" fn() -> *mut IPluginFactory>(b"GetPluginFactory\0")
    }
    .map_err(|_| {
        format!(
            "{} は VST3 プラグインではありません（GetPluginFactory がない）",
            binary.display()
        )
    })?;
    // SAFETY: GetPluginFactory は参照を 1 つ持ったファクトリを返す。
    let factory = unsafe { ComPtr::from_raw(get_factory()) }
        .ok_or_else(|| format!("{} がファクトリを返しませんでした", binary.display()))?;
    let module = Arc::new(Module { factory, _lib: lib });
    map.insert(path.to_path_buf(), module.clone());
    Ok(module)
}

impl Module {
    /// 中のクラスをすべて返す。
    pub(crate) fn classes(&self) -> Vec<ClassEntry> {
        let mut out = Vec::new();
        let f2 = self.factory.cast::<IPluginFactory2>();
        let mut factory_vendor = String::new();
        unsafe {
            let mut fi: PFactoryInfo = std::mem::zeroed();
            if self.factory.getFactoryInfo(&mut fi) == kResultOk {
                factory_vendor = from_c(&fi.vendor);
            }
            let n = self.factory.countClasses();
            for i in 0..n.max(0) {
                if let Some(f2) = &f2 {
                    let mut ci: PClassInfo2 = std::mem::zeroed();
                    if f2.getClassInfo2(i, &mut ci) == kResultOk {
                        let vendor = from_c(&ci.vendor);
                        out.push(ClassEntry {
                            cid: ci.cid.map(|b| b as u8),
                            name: from_c(&ci.name),
                            category: from_c(&ci.category),
                            sub_categories: from_c(&ci.subCategories),
                            vendor: if vendor.is_empty() {
                                factory_vendor.clone()
                            } else {
                                vendor
                            },
                            version: from_c(&ci.version),
                        });
                        continue;
                    }
                }
                let mut ci: PClassInfo = std::mem::zeroed();
                if self.factory.getClassInfo(i, &mut ci) == kResultOk {
                    out.push(ClassEntry {
                        cid: ci.cid.map(|b| b as u8),
                        name: from_c(&ci.name),
                        category: from_c(&ci.category),
                        sub_categories: String::new(),
                        vendor: factory_vendor.clone(),
                        version: String::new(),
                    });
                }
            }
        }
        out
    }

    /// クラス `cid` のオブジェクトを、インターフェース `I` として作る。
    pub(crate) fn create<I: Interface>(&self, cid: &[u8; 16]) -> Option<ComPtr<I>> {
        let mut obj = std::ptr::null_mut();
        let r = unsafe {
            self.factory.createInstance(
                cid.as_ptr() as *const c_char,
                I::IID.as_ptr() as *const c_char,
                &mut obj,
            )
        };
        if r != kResultOk {
            return None;
        }
        // SAFETY: createInstance は参照を 1 つ持ったオブジェクトを返す。
        unsafe { ComPtr::from_raw(obj as *mut I) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_ids_round_trip_through_hex() {
        let cid = [0x12, 0xAB, 0, 255, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let hex = cid_hex(&cid);
        assert_eq!(hex.len(), 32);
        assert_eq!(cid_from_hex(&hex), Some(cid));
        assert_eq!(cid_from_hex("xyz"), None);
    }
}
