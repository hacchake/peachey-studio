//! Window and event-loop runner.
//!
//! Draws only when input arrives or when the UI asks for another frame
//! (animation, playback), so an idle window uses no CPU or GPU time.
//!
//! # Accessibility
//!
//! The window is created hidden, an [`accesskit_winit::Adapter`] is attached
//! (it must see the window before it is first shown), and then the window is
//! shown. The adapter reports through the event loop:
//!
//! * *InitialTreeRequested*: a screen reader connected. The UI starts
//!   building its accessibility tree and the next frame sends all of it.
//! * *ActionRequested*: the screen reader asked for something (focus, click,
//!   increment, set a value...). The request is queued on the [`Ui`] and the
//!   next frame's widgets act on it.
//! * *AccessibilityDeactivated*: the screen reader went away; the tree stops
//!   being built, so an unused feature costs nothing.
//!
//! Set `PEEL_NO_A11Y=1` to skip creating the adapter.
//!
//! # Menus
//!
//! With [`WindowOptions::native_menus`], the app's [`App::menu_bar`] becomes
//! the platform menu bar on macOS and Windows (see `native_menu`), kept in
//! step after every frame. Choices arrive as user events and are queued on
//! the [`Ui`] for the next frame. Elsewhere, and when installing fails,
//! [`WindowControl::has_native_menu`] is false and the app draws its own.

use std::sync::Arc;
use std::time::{Duration, Instant};

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::{Window, WindowId};

use crate::geom::Rect;
use crate::menu::MenuBar;
use crate::render::Renderer;
use crate::ui::Ui;

/// Application callbacks.
pub trait App {
    /// Builds one frame.
    fn update(&mut self, ui: &mut Ui, window: &WindowControl);
    /// Called when the user asks to close the window. Return false to keep
    /// it open (for example to show an unsaved-changes prompt).
    fn close_requested(&mut self) -> bool {
        true
    }
    /// Called once before the process exits.
    fn shutdown(&mut self) {}
    /// The application menus, asked for after every frame. Only used when
    /// [`WindowOptions::native_menus`] is set.
    fn menu_bar(&mut self) -> Option<MenuBar> {
        None
    }
    /// 他のスレッドから [`App::wake`] を呼ぶための [`Waker`] を受け取る。
    /// アプリを作った直後に一度だけ呼ばれる。
    fn set_waker(&mut self, _waker: Waker) {}
    /// [`Waker`] が鳴ったときに、それが要求するフレームより先に呼ばれる。
    /// [`App::update`] と違い、ウィンドウが最小化・非表示でフレームが
    /// 描かれない間も呼ばれるので、他スレッドから積まれた仕事は
    /// `update` だけでなくここでも処理する。
    fn wake(&mut self) {}
}

/// イベントループに [`App::wake`] の呼び出しと再描画を頼む。複製は安く、
/// どのスレッドからでも使える。ウィンドウが閉じた後は何もしない。
#[derive(Clone)]
pub struct Waker(EventLoopProxy<UserEvent>);

impl Waker {
    pub fn wake(&self) {
        let _ = self.0.send_event(UserEvent::Wake);
    }
}

/// Window operations available to the app during a frame.
pub struct WindowControl {
    window: Option<Arc<Window>>,
    title: std::cell::RefCell<String>,
    drag: std::cell::Cell<bool>,
    exit: std::cell::Cell<bool>,
    adapter: String,
    native_menu: std::cell::Cell<bool>,
}

impl WindowControl {
    /// A control with no window, for driving an [`App`] in tests.
    pub fn headless() -> WindowControl {
        WindowControl {
            window: None,
            title: std::cell::RefCell::new(String::new()),
            drag: std::cell::Cell::new(false),
            exit: std::cell::Cell::new(false),
            adapter: "headless".into(),
            native_menu: std::cell::Cell::new(false),
        }
    }

    /// The app's menus are in the platform menu bar.
    pub fn has_native_menu(&self) -> bool {
        self.native_menu.get()
    }

    pub fn title(&self) -> String {
        self.title.borrow().clone()
    }

    pub fn exit_requested(&self) -> bool {
        self.exit.get()
    }

    pub fn set_title(&self, title: &str) {
        if *self.title.borrow() != title {
            if let Some(w) = &self.window {
                w.set_title(title);
            }
            *self.title.borrow_mut() = title.to_string();
        }
    }

    /// Starts moving the window with the pointer (for custom title bars).
    pub fn drag_window(&self) {
        self.drag.set(true);
    }

    pub fn exit(&self) {
        self.exit.set(true);
    }

    /// Space reserved at the top-left for native window controls when the
    /// content extends into the title bar.
    pub fn titlebar_inset(&self) -> f32 {
        if cfg!(target_os = "macos") { 78.0 } else { 0.0 }
    }

    pub fn adapter_info(&self) -> &str {
        &self.adapter
    }

    pub fn is_maximized(&self) -> bool {
        self.window.as_ref().is_some_and(|w| w.is_maximized())
    }

    pub fn toggle_maximized(&self) {
        if let Some(w) = &self.window {
            w.set_maximized(!w.is_maximized());
        }
    }
}

pub struct WindowOptions {
    pub title: String,
    pub size: (f64, f64),
    pub min_size: (f64, f64),
    /// PNG bytes for the window/taskbar icon (Windows and Linux; macOS uses
    /// the bundle icon).
    pub icon_png: Option<&'static [u8]>,
    /// Show [`App::menu_bar`] as the platform menu bar where there is one
    /// (macOS, Windows). On macOS this also replaces winit's default menu,
    /// whose Quit would end the process without asking the app.
    pub native_menus: bool,
}

impl Default for WindowOptions {
    fn default() -> Self {
        WindowOptions {
            title: "Peachey Studio".into(),
            size: (1440.0, 900.0),
            min_size: (1200.0, 720.0),
            icon_png: None,
            native_menus: false,
        }
    }
}

struct Running<A> {
    window: Arc<Window>,
    renderer: Renderer,
    control: WindowControl,
    app: A,
    adapter: Option<accesskit_winit::Adapter>,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    menu: Option<crate::native_menu::NativeMenu>,
}

/// Events the event loop receives from other threads.
#[derive(Debug)]
enum UserEvent {
    AccessKit(accesskit_winit::Event),
    /// A native menu item's command id.
    #[cfg_attr(not(any(target_os = "macos", target_os = "windows")), allow(dead_code))]
    Menu(u32),
    /// [`Waker`] が鳴った。
    Wake,
}

impl From<u32> for UserEvent {
    fn from(id: u32) -> UserEvent {
        UserEvent::Menu(id)
    }
}

impl From<accesskit_winit::Event> for UserEvent {
    fn from(e: accesskit_winit::Event) -> UserEvent {
        UserEvent::AccessKit(e)
    }
}

struct Runner<A, F> {
    options: WindowOptions,
    frames: u64,
    screenshot: Option<(std::path::PathBuf, u64)>,
    make: Option<F>,
    running: Option<Running<A>>,
    ui: Ui,
    start: Instant,
    error: Option<String>,
    proxy: EventLoopProxy<UserEvent>,
}

/// Opens the main window and runs until it closes. `make` builds the app
/// once the window and UI exist.
pub fn run<A: App + 'static, F: FnOnce(&mut Ui) -> A + 'static>(
    options: WindowOptions,
    make: F,
) -> Result<(), String> {
    let mut builder = EventLoop::<UserEvent>::with_user_event();
    #[cfg(target_os = "macos")]
    if options.native_menus {
        use winit::platform::macos::EventLoopBuilderExtMacOS;
        builder.with_default_menu(false);
    }
    let event_loop = builder.build().map_err(|e| e.to_string())?;
    event_loop.set_control_flow(ControlFlow::Wait);
    let screenshot = std::env::var_os("PEEL_SCREENSHOT").map(|p| {
        let frames = std::env::var("PEEL_SCREENSHOT_FRAMES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8);
        (std::path::PathBuf::from(p), frames)
    });
    let mut options = options;
    if let Some((w, h)) = std::env::var("PEEL_WINDOW_SIZE").ok().and_then(|v| {
        let (w, h) = v.split_once('x')?;
        Some((w.parse().ok()?, h.parse().ok()?))
    }) {
        options.size = (w, h);
    }
    let mut runner = Runner {
        options,
        frames: 0,
        screenshot,
        make: Some(make),
        running: None,
        ui: Ui::new(),
        start: Instant::now(),
        error: None,
        proxy: event_loop.create_proxy(),
    };
    event_loop.run_app(&mut runner).map_err(|e| e.to_string())?;
    match runner.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

impl<A: App, F: FnOnce(&mut Ui) -> A> ApplicationHandler<UserEvent> for Runner<A, F> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.running.is_some() {
            return;
        }
        #[allow(unused_mut)]
        let mut attrs = Window::default_attributes()
            .with_title(&self.options.title)
            // Hidden until the accessibility adapter has seen the window.
            .with_visible(false)
            .with_inner_size(LogicalSize::new(self.options.size.0, self.options.size.1))
            .with_min_inner_size(LogicalSize::new(
                self.options.min_size.0,
                self.options.min_size.1,
            ));
        if let Some(icon) = self.options.icon_png.and_then(decode_icon) {
            // Windows では小さいアイコンとは別に、タスクバーや Alt+Tab に
            // 出る大きいアイコンも渡す。
            #[cfg(target_os = "windows")]
            {
                use winit::platform::windows::WindowAttributesExtWindows;
                attrs = attrs.with_taskbar_icon(Some(icon.clone()));
            }
            attrs = attrs.with_window_icon(Some(icon));
        }
        #[cfg(target_os = "macos")]
        {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs = attrs
                .with_titlebar_transparent(true)
                .with_fullsize_content_view(true)
                .with_title_hidden(true);
        }
        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.error = Some(format!("could not create window: {e}"));
                event_loop.exit();
                return;
            }
        };
        let adapter = std::env::var_os("PEEL_NO_A11Y").is_none().then(|| {
            accesskit_winit::Adapter::with_event_loop_proxy(event_loop, &window, self.proxy.clone())
        });
        window.set_visible(true);
        let renderer = match pollster::block_on(Renderer::new(
            window.clone(),
            event_loop.owned_display_handle(),
        )) {
            Ok(r) => r,
            Err(e) => {
                self.error = Some(e.to_string());
                event_loop.exit();
                return;
            }
        };
        log::info!("graphics adapter: {}", renderer.adapter_info());
        let control = WindowControl {
            window: Some(window.clone()),
            title: std::cell::RefCell::new(self.options.title.clone()),
            drag: std::cell::Cell::new(false),
            exit: std::cell::Cell::new(false),
            adapter: renderer.adapter_info().to_string(),
            native_menu: std::cell::Cell::new(false),
        };
        let make = self.make.take().expect("app factory");
        #[allow(unused_mut)]
        let mut app = make(&mut self.ui);
        app.set_waker(Waker(self.proxy.clone()));
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let menu = if self.options.native_menus
            && let Some(bar) = app.menu_bar()
        {
            crate::native_menu::forward_events(self.proxy.clone());
            let menu = crate::native_menu::NativeMenu::install(&bar, &window);
            if menu.is_none() {
                log::warn!("the platform menu bar could not be installed");
                crate::native_menu::stop_events();
            }
            control.native_menu.set(menu.is_some());
            menu
        } else {
            None
        };
        self.running = Some(Running {
            window: window.clone(),
            renderer,
            control,
            app,
            adapter,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            menu,
        });
        window.request_redraw();
    }

    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if matches!(cause, StartCause::ResumeTimeReached { .. })
            && let Some(run) = &self.running
        {
            run.window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        let event = match event {
            UserEvent::AccessKit(event) => event,
            UserEvent::Menu(id) => {
                self.ui.queue_menu_command(id);
                if let Some(run) = &self.running {
                    run.window.request_redraw();
                }
                return;
            }
            UserEvent::Wake => {
                // Windows は最小化中のウィンドウに描画メッセージを送らない。
                // ここで頼む再描画を待たず、先に仕事を済ませる。
                if let Some(run) = &mut self.running {
                    run.app.wake();
                    run.window.request_redraw();
                }
                return;
            }
        };
        match event.window_event {
            accesskit_winit::WindowEvent::InitialTreeRequested => {
                self.ui.set_accessibility_active(true);
            }
            accesskit_winit::WindowEvent::ActionRequested(request) => {
                self.ui.queue_accessibility_action(request);
            }
            accesskit_winit::WindowEvent::AccessibilityDeactivated => {
                self.ui.set_accessibility_active(false);
            }
        }
        if let Some(run) = &self.running {
            run.window.request_redraw();
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(run) = &mut self.running else { return };
        let now = self.start.elapsed().as_secs_f64();
        if let Some(adapter) = &mut run.adapter {
            adapter.process_event(&run.window, &event);
        }
        match &event {
            WindowEvent::CloseRequested => {
                if run.app.close_requested() {
                    run.app.shutdown();
                    event_loop.exit();
                } else {
                    // Show whatever the app put up instead (a save prompt).
                    run.window.request_redraw();
                }
                return;
            }
            WindowEvent::Resized(size) => {
                run.renderer.resize(size.width, size.height);
                run.window.request_redraw();
            }
            WindowEvent::ScaleFactorChanged { .. } => run.window.request_redraw(),
            WindowEvent::RedrawRequested => {
                self.frame(event_loop, now);
                return;
            }
            _ => {}
        }
        self.ui.input.set_scale(run.window.scale_factor() as f32);
        self.ui.input.handle(&event, now);
        run.window.request_redraw();
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {}

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(run) = &mut self.running {
            run.app.shutdown();
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if run.menu.take().is_some() {
                crate::native_menu::stop_events();
            }
        }
    }
}

impl<A: App, F: FnOnce(&mut Ui) -> A> Runner<A, F> {
    fn frame(&mut self, event_loop: &ActiveEventLoop, now: f64) {
        let Some(run) = &mut self.running else { return };
        let scale = run.window.scale_factor() as f32;
        let size = run.window.inner_size();
        // 最小化すると Windows は大きさを 0 にする。その大きさで画面を組むと
        // 文字の行の高さが 0 になり、cosmic-text の検査で落ちる。見えない
        // 間は描かず、元に戻したときの Resized で再描画する。MCP などの
        // 仕事は App::wake が受け持つ。
        if size.width == 0 || size.height == 0 || run.window.is_minimized() == Some(true) {
            event_loop.set_control_flow(ControlFlow::Wait);
            return;
        }
        let screen = Rect {
            x: 0.0,
            y: 0.0,
            w: size.width as f32 / scale,
            h: size.height as f32 / scale,
        };
        self.ui.begin_frame(screen, scale, now);
        run.app.update(&mut self.ui, &run.control);
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        if let Some(menu) = &mut run.menu
            && let Some(bar) = run.app.menu_bar()
        {
            menu.sync(&bar);
        }
        if self.ui.accessibility_active() {
            self.ui.set_window_label(&run.control.title());
        }
        self.ui.end_frame();
        if self.ui.accessibility_active()
            && let Some(update) = self.ui.take_accessibility_update()
            && let Some(adapter) = &mut run.adapter
        {
            let mut taken = false;
            adapter.update_if_active(|| {
                taken = true;
                update
            });
            // The adapter had already gone inactive: send everything next
            // time.
            if !taken {
                self.ui.accessibility_resync();
            }
        }
        run.window.set_cursor(self.ui.cursor());
        let clear = self.ui.theme.bg;
        self.frames += 1;
        if let Some((path, at)) = &self.screenshot
            && self.frames >= *at
        {
            let (w, h, pixels) = run
                .renderer
                .capture(&self.ui.scene, &mut self.ui.text, clear);
            match write_png(path, w, h, &pixels) {
                Ok(()) => log::info!("screenshot written to {}", path.display()),
                Err(e) => log::error!("screenshot failed: {e}"),
            }
            run.app.shutdown();
            event_loop.exit();
            return;
        }
        let ok = run
            .renderer
            .render(&self.ui.scene, &mut self.ui.text, clear);
        if run.control.drag.replace(false)
            && let Err(e) = run.window.drag_window()
        {
            log::debug!("drag_window: {e}");
        }
        if run.control.exit.get() {
            run.app.shutdown();
            event_loop.exit();
            return;
        }
        if !ok || self.ui.wants_repaint() || self.screenshot.is_some() {
            event_loop.set_control_flow(ControlFlow::Wait);
            run.window.request_redraw();
        } else {
            let wake = self
                .ui
                .next_repaint_at()
                .and_then(|at| Duration::try_from_secs_f64(at).ok())
                .and_then(|after| self.start.checked_add(after));
            event_loop.set_control_flow(wake.map_or(ControlFlow::Wait, ControlFlow::WaitUntil));
        }
    }
}

fn write_png(path: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())
}

fn decode_icon(png_bytes: &[u8]) -> Option<winit::window::Icon> {
    let decoder = png::Decoder::new(std::io::Cursor::new(png_bytes));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    buf.truncate(info.buffer_size());
    winit::window::Icon::from_rgba(buf, info.width, info.height).ok()
}
