//! プラグインの画面（エディタ）を入れる、独立したウィンドウ。
//!
//! VST3 と CLAP のホストが共通で使う。Windows では Win32 のウィンドウを作り、
//! プラグインはその中に自分の画面を子ウィンドウとして付ける。ウィンドウは
//! アプリのウィンドウと同じスレッドで作るので、メッセージはアプリのイベント
//! ループがそのまま配る。
//!
//! 閉じるボタンではウィンドウを隠して印を付けるだけにする。プラグインの
//! 画面を外す後片付けは、ホストが [`PluginWindow::is_closed`] を見て、
//! プラグインを呼んでよい場面で行う。

#[cfg(not(windows))]
use std::ffi::c_void;

/// 利用者がウィンドウの大きさを変えたときに呼ばれる（クライアント領域の幅と高さ）。
pub type ResizeFn = Box<dyn Fn(i32, i32) + Send + Sync>;

#[cfg(windows)]
pub use imp::{PluginWindow, WindowSizer};

#[cfg(not(windows))]
pub struct PluginWindow;

#[cfg(not(windows))]
#[derive(Clone)]
pub struct WindowSizer;

#[cfg(not(windows))]
impl WindowSizer {
    pub fn set_client_size(&self, _width: i32, _height: i32) {}
}

#[cfg(not(windows))]
impl PluginWindow {
    pub fn create(
        _title: &str,
        _resizable: bool,
        _on_user_resize: ResizeFn,
    ) -> Result<PluginWindow, String> {
        Err("プラグインの画面は今のところ Windows だけで開けます".into())
    }
    pub fn handle(&self) -> *mut c_void {
        std::ptr::null_mut()
    }
    pub fn scale(&self) -> f64 {
        1.0
    }
    pub fn sizer(&self) -> WindowSizer {
        WindowSizer
    }
    pub fn set_client_size(&self, _width: i32, _height: i32) {}
    pub fn show(&self) {}
    pub fn hide(&self) {}
    pub fn is_closed(&self) -> bool {
        true
    }
    pub fn destroy(self) {}
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
    use std::sync::{Arc, OnceLock};

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;
    use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
    use windows_sys::Win32::UI::WindowsAndMessaging::*;

    use super::ResizeFn;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    struct WinShared {
        hwnd: AtomicPtr<c_void>,
        closed: AtomicBool,
        /// 自分で大きさを変えている最中（利用者の操作として伝え直さない）。
        resizing: AtomicBool,
        style: u32,
        on_user_resize: ResizeFn,
    }

    impl WinShared {
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

    unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const WinShared;
            if let Some(s) = p.as_ref() {
                match msg {
                    WM_CLOSE => {
                        // 後片付けはホストに任せ、ここでは隠すだけ。
                        s.closed.store(true, Ordering::Relaxed);
                        ShowWindow(hwnd, SW_HIDE);
                        return 0;
                    }
                    WM_SIZE if !s.resizing.load(Ordering::Relaxed) => {
                        let mut rc: RECT = std::mem::zeroed();
                        GetClientRect(hwnd, &mut rc);
                        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
                        if w > 0 && h > 0 {
                            (s.on_user_resize)(w, h);
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

    /// ほかのスレッドやコールバックから大きさだけ変える口。
    #[derive(Clone)]
    pub struct WindowSizer(Arc<WinShared>);

    impl WindowSizer {
        pub fn set_client_size(&self, width: i32, height: i32) {
            self.0.set_client_size(width, height);
        }
    }

    /// プラグインの画面を入れるウィンドウ。メインスレッドで作り、触り、閉じる。
    pub struct PluginWindow {
        shared: Arc<WinShared>,
    }

    // SAFETY: メインスレッドでしか作らず、触らず、閉じない。
    unsafe impl Send for PluginWindow {}
    unsafe impl Send for WinShared {}
    unsafe impl Sync for WinShared {}

    impl PluginWindow {
        /// 隠れた状態で作る。大きさを決めてから [`PluginWindow::show`] で出す。
        pub fn create(
            title: &str,
            resizable: bool,
            on_user_resize: ResizeFn,
        ) -> Result<PluginWindow, String> {
            let mut style =
                WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
            if resizable {
                style |= WS_THICKFRAME | WS_MAXIMIZEBOX;
            }
            let title = wide(title);
            unsafe {
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
                    closed: AtomicBool::new(false),
                    resizing: AtomicBool::new(false),
                    style,
                    on_user_resize,
                });
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, Arc::as_ptr(&shared) as isize);
                Ok(PluginWindow { shared })
            }
        }

        /// Win32 のウィンドウハンドル（HWND）。
        pub fn handle(&self) -> *mut c_void {
            self.shared.hwnd.load(Ordering::Relaxed)
        }

        /// 画面の拡大率（96 dpi を 1.0 とする）。
        pub fn scale(&self) -> f64 {
            let hwnd = self.handle();
            if hwnd.is_null() {
                return 1.0;
            }
            unsafe { GetDpiForWindow(hwnd) as f64 / 96.0 }
        }

        pub fn sizer(&self) -> WindowSizer {
            WindowSizer(self.shared.clone())
        }

        pub fn set_client_size(&self, width: i32, height: i32) {
            self.shared.set_client_size(width, height);
        }

        /// 見せて手前に出す（最小化していれば戻す）。
        pub fn show(&self) {
            let hwnd = self.handle();
            unsafe {
                if IsIconic(hwnd) != 0 {
                    ShowWindow(hwnd, SW_RESTORE);
                }
                ShowWindow(hwnd, SW_SHOW);
                SetForegroundWindow(hwnd);
            }
            self.shared.closed.store(false, Ordering::Relaxed);
        }

        pub fn hide(&self) {
            unsafe { ShowWindow(self.handle(), SW_HIDE) };
        }

        /// 閉じるボタンが押された。
        pub fn is_closed(&self) -> bool {
            self.shared.closed.load(Ordering::Relaxed)
        }

        /// ウィンドウを壊す。プラグインの画面は先に外しておく。
        pub fn destroy(self) {
            let hwnd = self
                .shared
                .hwnd
                .swap(std::ptr::null_mut(), Ordering::Relaxed);
            if !hwnd.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    DestroyWindow(hwnd);
                }
            }
        }
    }

    impl Drop for PluginWindow {
        fn drop(&mut self) {
            let hwnd = self
                .shared
                .hwnd
                .swap(std::ptr::null_mut(), Ordering::Relaxed);
            if !hwnd.is_null() {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                    DestroyWindow(hwnd);
                }
            }
        }
    }
}
