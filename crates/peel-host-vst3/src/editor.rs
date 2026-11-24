//! プラグインの画面（エディタ）を、独立したウィンドウで開く。
//!
//! ウィンドウそのものは `peel-plugin-window` が用意する。閉じるボタンでは
//! ウィンドウが隠れて印が付くだけで、画面を外す後片付けは
//! [`crate::Vst3Handle::idle`] で行う。

use std::sync::Arc;
use std::sync::atomic::{AtomicPtr, Ordering};

use peel_plugin_window::{PluginWindow, WindowSizer};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{ComPtr, ComWrapper};

use crate::com::{PlugFrame, ResizeSink};
use crate::plugin::Plugin;

struct FrameSink(WindowSizer);

impl ResizeSink for FrameSink {
    fn resize(&self, width: i32, height: i32) -> bool {
        self.0.set_client_size(width, height);
        true
    }
}

/// 利用者がウィンドウの大きさを変えたときに画面へ伝える先。
struct ViewPtr(AtomicPtr<IPlugView>);

// SAFETY: ウィンドウ手続き（メインスレッド）からしか使わない。
unsafe impl Send for ViewPtr {}
unsafe impl Sync for ViewPtr {}

pub(crate) struct Editor {
    window: PluginWindow,
    view: ComPtr<IPlugView>,
    view_ptr: Arc<ViewPtr>,
    _frame: ComWrapper<PlugFrame>,
}

// SAFETY: メインスレッドでしか作らず、触らず、閉じない。
unsafe impl Send for Editor {}

impl Editor {
    pub(crate) fn open(plugin: &Arc<Plugin>) -> Result<Editor, String> {
        if !cfg!(windows) {
            return Err("プラグインの画面は今のところ Windows だけで開けます".into());
        }
        let name = &plugin.info.name;
        let controller = plugin
            .controller
            .as_ref()
            .ok_or_else(|| format!("{name} には画面がありません"))?;
        let view = unsafe { ComPtr::from_raw(controller.createView(ViewType::kEditor)) }
            .ok_or_else(|| format!("{name} は画面を作りませんでした"))?;
        unsafe {
            if view.isPlatformTypeSupported(kPlatformTypeHWND) != kResultTrue {
                return Err(format!(
                    "{name} の画面は Windows のウィンドウに対応していません"
                ));
            }
            let view_ptr = Arc::new(ViewPtr(AtomicPtr::new(std::ptr::null_mut())));
            let on_resize = {
                let view_ptr = view_ptr.clone();
                Box::new(move |w: i32, h: i32| {
                    let v = view_ptr.0.load(Ordering::Relaxed);
                    if !v.is_null() {
                        let mut r = ViewRect {
                            left: 0,
                            top: 0,
                            right: w,
                            bottom: h,
                        };
                        ((*(*v).vtbl).onSize)(v, &mut r);
                    }
                })
            };
            let window = PluginWindow::create(name, view.canResize() == kResultTrue, on_resize)?;
            let frame = ComWrapper::new(PlugFrame {
                sink: Arc::new(FrameSink(window.sizer())),
            });
            if let Some(f) = frame.as_com_ref::<IPlugFrame>() {
                view.setFrame(f.as_ptr());
            }
            // 高 DPI の画面では、拡大率を先に伝える。
            if let Some(scale) = view.cast::<IPlugViewContentScaleSupport>() {
                scale.setContentScaleFactor(window.scale() as f32);
            }
            if view.attached(window.handle(), kPlatformTypeHWND) != kResultOk {
                view.setFrame(std::ptr::null_mut());
                window.destroy();
                return Err(format!("{name} の画面を付けられませんでした"));
            }
            view_ptr.0.store(view.as_ptr(), Ordering::Relaxed);
            let mut r: ViewRect = std::mem::zeroed();
            if view.getSize(&mut r) == kResultOk {
                window.set_client_size(r.right - r.left, r.bottom - r.top);
            }
            window.show();
            Ok(Editor {
                window,
                view,
                view_ptr,
                _frame: frame,
            })
        }
    }

    /// 閉じるボタンが押された。
    pub(crate) fn is_closed(&self) -> bool {
        self.window.is_closed()
    }

    /// 手前に出す。
    pub(crate) fn raise(&self) {
        self.window.show();
    }

    pub(crate) fn close(self) {
        self.view_ptr
            .0
            .store(std::ptr::null_mut(), Ordering::Relaxed);
        unsafe {
            self.view.removed();
            self.view.setFrame(std::ptr::null_mut());
        }
        self.window.destroy();
    }
}
