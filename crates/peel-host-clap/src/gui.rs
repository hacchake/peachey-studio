//! プラグイン自身の画面（`clap.gui`）を、独立したウィンドウに埋め込んで開く。
//!
//! ウィンドウは `peel-plugin-window` が用意し、プラグインには Win32 の子として
//! 付けてもらう（浮動ウィンドウは使わない）。画面の呼び出しはすべてメイン
//! スレッドで、ほかの main 系の呼び出しと同じロックの下で行う。
//!
//! プラグインからの頼み（大きさを変えたい、見せたい、隠したい、閉じた）は
//! ホストのコールバックで印を付けるだけにし、[`idle`] で反映する。

use std::sync::atomic::Ordering;
use std::sync::{Arc, PoisonError, Weak};

use clap_sys::ext::gui::{CLAP_WINDOW_API_WIN32, clap_plugin_gui, clap_window, clap_window_handle};
use peel_plugin_window::PluginWindow;

use crate::host::{F_GUI_CLOSED, F_GUI_HIDE, F_GUI_RESIZE, F_GUI_SHOW};
use crate::instance::PluginShared;
use crate::thread::RoleScope;

/// 開いている画面 1 つ。
pub(crate) struct ClapEditor {
    window: PluginWindow,
}

fn gui_of(shared: &PluginShared) -> Option<&clap_plugin_gui> {
    // SAFETY: 拡張の表はプラグインが生きている間有効（または null）。
    unsafe { shared.exts.gui.as_ref() }
}

/// 画面を持っているか（Windows のときだけ開ける）。
pub(crate) fn available(shared: &PluginShared) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let Some(gui) = gui_of(shared) else {
        return false;
    };
    let Some(supported) = gui.is_api_supported else {
        return false;
    };
    let _guard = shared.lock_main();
    let _scope = RoleScope::main();
    // SAFETY: メインスレッドからの呼び出し。
    unsafe { supported(shared.plugin, CLAP_WINDOW_API_WIN32.as_ptr(), false) }
}

pub(crate) fn is_open(shared: &PluginShared) -> bool {
    shared
        .editor
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|e| !e.window.is_closed())
}

/// 画面を開く。開いていれば手前に出す。
pub(crate) fn open(shared: &Arc<PluginShared>, title: &str) -> Result<(), String> {
    let mut slot = shared.editor.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(e) = slot.as_ref() {
        if !e.window.is_closed() {
            e.window.show();
            return Ok(());
        }
        if let Some(e) = slot.take() {
            destroy(shared, e);
        }
    }
    let gui = gui_of(shared).ok_or_else(|| format!("{title} には画面がありません"))?;
    let (Some(supported), Some(create), Some(set_parent)) =
        (gui.is_api_supported, gui.create, gui.set_parent)
    else {
        return Err(format!("{title} の画面の作りが足りません"));
    };
    let api = CLAP_WINDOW_API_WIN32.as_ptr();
    let weak: Weak<PluginShared> = Arc::downgrade(shared);
    let on_resize = Box::new(move |w: i32, h: i32| {
        if let Some(s) = weak.upgrade() {
            user_resized(&s, w, h);
        }
    });
    let guard = shared.lock_main();
    let _scope = RoleScope::main();
    let p = shared.plugin;
    // SAFETY: すべてメインスレッドで、プラグインが生きている間の呼び出し。
    unsafe {
        if !supported(p, api, false) {
            return Err(format!(
                "{title} の画面は Windows のウィンドウに対応していません"
            ));
        }
        if !create(p, api, false) {
            return Err(format!("{title} は画面を作りませんでした"));
        }
        let resizable = gui.can_resize.is_some_and(|f| f(p));
        let window = match PluginWindow::create(title, resizable, on_resize) {
            Ok(w) => w,
            Err(e) => {
                if let Some(destroy) = gui.destroy {
                    destroy(p);
                }
                return Err(e);
            }
        };
        // Win32 は物理ピクセルなので、拡大率を伝える（断られてもよい）。
        if let Some(set_scale) = gui.set_scale {
            set_scale(p, window.scale());
        }
        let parent = clap_window {
            api,
            specific: clap_window_handle {
                win32: window.handle(),
            },
        };
        if !set_parent(p, &parent) {
            if let Some(destroy) = gui.destroy {
                destroy(p);
            }
            window.destroy();
            return Err(format!("{title} の画面を付けられませんでした"));
        }
        let (mut w, mut h) = (0u32, 0u32);
        if gui.get_size.is_some_and(|f| f(p, &mut w, &mut h)) {
            window.set_client_size(w as i32, h as i32);
        }
        if let Some(show) = gui.show {
            show(p);
        }
        window.show();
        drop(guard);
        *slot = Some(ClapEditor { window });
    }
    Ok(())
}

/// 利用者がウィンドウの大きさを変えた。プラグインに合わせてもらう。
fn user_resized(shared: &PluginShared, w: i32, h: i32) {
    let Some(gui) = gui_of(shared) else { return };
    // ウィンドウ手続きの中なので、ロック中なら諦める（行き詰まりを避ける）。
    let Ok(_guard) = shared.try_lock_main() else {
        return;
    };
    let _scope = RoleScope::main();
    let p = shared.plugin;
    let (mut w, mut h) = (w.max(1) as u32, h.max(1) as u32);
    // SAFETY: メインスレッドからの呼び出し。
    unsafe {
        if let Some(adjust) = gui.adjust_size {
            adjust(p, &mut w, &mut h);
        }
        if let Some(set_size) = gui.set_size {
            set_size(p, w, h);
        }
    }
}

/// 毎フレーム。プラグインの頼みを反映し、閉じられた画面を片付ける。
pub(crate) fn idle(shared: &PluginShared) {
    let host = &shared.host.shared;
    let flags = host.take(F_GUI_RESIZE | F_GUI_SHOW | F_GUI_HIDE | F_GUI_CLOSED);
    let mut slot = shared.editor.lock().unwrap_or_else(PoisonError::into_inner);
    let Some(editor) = slot.as_ref() else {
        return;
    };
    if flags & F_GUI_RESIZE != 0 {
        let size = host.gui_size.load(Ordering::Acquire);
        editor
            .window
            .set_client_size((size >> 32) as i32, (size & 0xFFFF_FFFF) as i32);
    }
    if flags & F_GUI_SHOW != 0 {
        editor.window.show();
    }
    if flags & F_GUI_HIDE != 0 {
        editor.window.hide();
    }
    if (flags & F_GUI_CLOSED != 0 || editor.window.is_closed())
        && let Some(e) = slot.take()
    {
        destroy(shared, e);
    }
}

pub(crate) fn close(shared: &PluginShared) {
    let taken = shared
        .editor
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take();
    if let Some(e) = taken {
        destroy(shared, e);
    }
}

fn destroy(shared: &PluginShared, editor: ClapEditor) {
    let _guard = shared.lock_main();
    destroy_unlocked(shared, editor);
}

/// ロックを取らずに片付ける（呼び手が取っているか、もう誰も触れないとき）。
pub(crate) fn destroy_unlocked(shared: &PluginShared, editor: ClapEditor) {
    let _scope = RoleScope::main();
    if let Some(gui) = gui_of(shared) {
        // SAFETY: メインスレッドからの呼び出し。
        unsafe {
            if let Some(hide) = gui.hide {
                hide(shared.plugin);
            }
            if let Some(destroy) = gui.destroy {
                destroy(shared.plugin);
            }
        }
    }
    editor.window.destroy();
}
