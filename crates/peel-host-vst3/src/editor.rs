//! プラグインの画面（エディタ）を、独立したウィンドウで開く。
//!
//! Windows では Win32 のウィンドウを作り、プラグインの画面をその中に付ける。
//! ウィンドウはアプリのウィンドウと同じスレッドで作るので、メッセージは
//! アプリのイベントループがそのまま配る。閉じるボタンではウィンドウを
//! 隠して印を付けるだけにし、後片付けは [`crate::Vst3Handle::idle`] で行う。

#[cfg(not(windows))]
use std::sync::Arc;

#[cfg(not(windows))]
use crate::plugin::Plugin;

#[cfg(windows)]
pub(crate) use imp::Editor;

#[cfg(not(windows))]
pub(crate) struct Editor;

#[cfg(not(windows))]
impl Editor {
    pub(crate) fn open(_plugin: &Arc<Plugin>) -> Result<Editor, String> {
        Err("プラグインの画面は今のところ Windows だけで開けます".into())
    }
    pub(crate) fn is_closed(&self) -> bool {
        true
    }
    pub(crate) fn raise(&self) {}
    pub(crate) fn close(self) {}
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::sync::{Arc, OnceLock};

    use vst3::Steinberg::Vst::*;
    use vst3::Steinberg::*;
    use vst3::{ComPtr, ComWrapper};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use crate::com::{PlugFrame, ResizeSink};
    use crate::plugin::Plugin;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// ウィンドウとプラグインの画面が共有する値。
    struct WinShared {
        hwnd: AtomicPtr<c_void>,
        view: AtomicPtr<IPlugView>,
        closed: AtomicBool,
        /// 自分で大きさを変えている最中（WM_SIZE を画面に伝え直さない）。
        resizing: AtomicBool,
        style: u32,
    }

    // SAFETY: 中身はメインスレッドでしか触らない。原子変数にしているのは
    // ウィンドウ手続きからも読むため。
    unsafe impl Send for WinShared {}
    unsafe impl Sync for WinShared {}

    impl WinShared {
        /// クライアント領域を `w`×`h` にする。
        fn set_client_size(&self, w: i32, h: i32) {
            let hwnd = self.hwnd.load(Ordering::Relaxed);
            if hwnd.is_null() || w <= 0 || h <= 0 {
                return;
            }
            unsafe {
                let mut rc = RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                };
                AdjustWindowRectExForDpi(&mut rc, self.style, 0, 0, GetDpiForWindow(hwnd));
                self.resizing.store(true, Ordering::Relaxed);
                SetWindowPos(
                    hwnd,
                    std::ptr::null_mut(),
                    0,
                    0,
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
                self.resizing.store(false, Ordering::Relaxed);
            }
        }
    }

    struct FrameSink(Arc<WinShared>);

    impl ResizeSink for FrameSink {
        fn resize(&self, width: i32, height: i32) -> bool {
            self.0.set_client_size(width, height);
            true
        }
    }

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WinShared;
            if let Some(s) = p.as_ref() {
                match msg {
                    WM_CLOSE => {
                        // 後片付けはメインの idle で。ここでは隠すだけ。
                        s.closed.store(true, Ordering::Relaxed);
                        ShowWindow(hwnd, SW_HIDE);
                        return 0;
                    }
                    WM_SIZE if !s.resizing.load(Ordering::Relaxed) => {
                        let view = s.view.load(Ordering::Relaxed);
                        if !view.is_null() {
                            let mut rc: RECT = std::mem::zeroed();
                            GetClientRect(hwnd, &mut rc);
                            let mut r = ViewRect {
                                left: 0,
                                top: 0,
                                right: rc.right - rc.left,
                                bottom: rc.bottom - rc.top,
                            };
                            if r.right > 0 && r.bottom > 0 {
                                ((*(*view).vtbl).onSize)(view, &mut r);
                            }
                        }
                    }
                    _ => {}
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }

    fn class_name() -> &'static [u16] {
        static CLASS: OnceLock<Vec<u16>> = OnceLock::new();
        CLASS.get_or_init(|| {
            let name = wide("PeacheyStudioPluginWindow");
            unsafe {
                let wc = WNDCLASSEXW {
                    cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                    style: CS_HREDRAW | CS_VREDRAW,
                    lpfnWndProc: Some(wndproc),
                    cbClsExtra: 0,
                    cbWndExtra: 0,
                    hInstance: GetModuleHandleW(std::ptr::null()),
                    hIcon: std::ptr::null_mut(),
                    hCursor: LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
                    hbrBackground: std::ptr::null_mut(),
                    lpszMenuName: std::ptr::null(),
                    lpszClassName: name.as_ptr(),
                    hIconSm: std::ptr::null_mut(),
                };
                RegisterClassExW(&wc);
            }
            name
        })
    }

    /// 今アクティブなこのアプリのウィンドウ（プラグインの画面をその上に保つ）。
    fn app_window() -> HWND {
        unsafe {
            let fg = GetForegroundWindow();
            let mut pid = 0;
            GetWindowThreadProcessId(fg, &mut pid);
            if pid == GetCurrentProcessId() {
                fg
            } else {
                std::ptr::null_mut()
            }
        }
    }

    pub(crate) struct Editor {
        shared: Arc<WinShared>,
        view: ComPtr<IPlugView>,
        _frame: ComWrapper<PlugFrame>,
    }

    // SAFETY: メインスレッドでしか作らず、触らず、閉じない。
    unsafe impl Send for Editor {}

    impl Editor {
        pub(crate) fn open(plugin: &Arc<Plugin>) -> Result<Editor, String> {
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
                let resizable = view.canResize() == kResultTrue;
                let mut style =
                    WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
                if resizable {
                    style |= WS_THICKFRAME | WS_MAXIMIZEBOX;
                }
                let title = wide(name);
                let hwnd = CreateWindowExW(
                    0,
                    class_name().as_ptr(),
                    title.as_ptr(),
                    style,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    640,
                    480,
                    app_window(),
                    std::ptr::null_mut(),
                    GetModuleHandleW(std::ptr::null()),
                    std::ptr::null(),
                );
                if hwnd.is_null() {
                    return Err("プラグインのウィンドウを作れませんでした".into());
                }
                let shared = Arc::new(WinShared {
                    hwnd: AtomicPtr::new(hwnd),
                    view: AtomicPtr::new(std::ptr::null_mut()),
                    closed: AtomicBool::new(false),
                    resizing: AtomicBool::new(false),
                    style,
                });
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&shared) as isize);
                let frame = ComWrapper::new(PlugFrame {
                    sink: Arc::new(FrameSink(shared.clone())),
                });
                if let Some(f) = frame.as_com_ref::<IPlugFrame>() {
                    view.setFrame(f.as_ptr());
                }
                // 高 DPI の画面では、拡大率を先に伝える。
                if let Some(scale) = view.cast::<IPlugViewContentScaleSupport>() {
                    scale.setContentScaleFactor(GetDpiForWindow(hwnd) as f32 / 96.0);
                }
                if view.attached(hwnd, kPlatformTypeHWND) != kResultOk {
                    view.setFrame(std::ptr::null_mut());
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    DestroyWindow(hwnd);
                    return Err(format!("{name} の画面を付けられませんでした"));
                }
                shared.view.store(view.as_ptr(), Ordering::Relaxed);
                let mut r: ViewRect = std::mem::zeroed();
                if view.getSize(&mut r) == kResultOk {
                    shared.set_client_size(r.right - r.left, r.bottom - r.top);
                }
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
                Ok(Editor {
                    shared,
                    view,
                    _frame: frame,
                })
            }
        }

        /// 閉じるボタンが押された。
        pub(crate) fn is_closed(&self) -> bool {
            self.shared.closed.load(Ordering::Relaxed)
        }

        /// 手前に出す。
        pub(crate) fn raise(&self) {
            let hwnd = self.shared.hwnd.load(Ordering::Relaxed);
            unsafe {
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
            }
        }

        pub(crate) fn close(self) {
            let hwnd = self
                .shared
                .hwnd
                .swap(std::ptr::null_mut(), Ordering::Relaxed);
            unsafe {
                self.shared
                    .view
                    .store(std::ptr::null_mut(), Ordering::Relaxed);
                self.view.removed();
                self.view.setFrame(std::ptr::null_mut());
                if !hwnd.is_null() {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    DestroyWindow(hwnd);
                }
            }
        }
    }
}
