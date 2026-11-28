//! MCP の通信に使う標準出力を、プラグインの書き込みから守る。
//!
//! stdio の MCP では、標準出力の 1 行 1 行が JSON-RPC の通信文になる。
//! ところが同じプロセスに読み込んだ VST3/CLAP プラグインの中には、C の
//! `printf` などで標準出力に自分のログを書くものがある。CRT はそれをため込んで
//! 好きな時に吐き出すので、応答の行の途中や頭に文字が混ざり、相手はその応答を
//! 読めなくなる（書き出しを何回か続けたところで、決まって返事が来なくなる）。
//!
//! そこで、プラグインを読み込む前に本物の標準出力を複製して通信専用にしまい、
//! プロセスの標準出力（OS の std ハンドルと CRT の fd 1）は標準エラーへ向け直す。
//! こうすると、誰が標準出力に何を書いても、通信には混ざらずログに流れる。

use std::fs::File;
use std::sync::Mutex;

static PROTOCOL_OUT: Mutex<Option<Option<File>>> = Mutex::new(None);

/// 本物の標準出力を通信用に取り分け、プロセスの標準出力を標準エラーへ向ける。
/// プラグインを読み込む前（起動直後）に呼ぶ。2 回目以降は何もしない。
pub fn isolate_stdout() {
    let mut slot = PROTOCOL_OUT.lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        return;
    }
    let file = duplicate_stdout();
    if file.is_some() {
        redirect_stdout_to_stderr();
    }
    *slot = Some(file);
}

/// 通信用に取り分けた標準出力。取り分けに失敗していれば `None`
/// （そのときはふつうの標準出力を使う）。
pub(crate) fn take_protocol_stdout() -> Option<File> {
    isolate_stdout();
    PROTOCOL_OUT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_mut()
        .and_then(Option::take)
}

#[cfg(windows)]
fn duplicate_stdout() -> Option<File> {
    use std::os::windows::io::AsHandle;
    let handle = std::io::stdout().as_handle().try_clone_to_owned().ok()?;
    Some(File::from(handle))
}

#[cfg(unix)]
fn duplicate_stdout() -> Option<File> {
    use std::os::fd::AsFd;
    let fd = std::io::stdout().as_fd().try_clone_to_owned().ok()?;
    Some(File::from(fd))
}

#[cfg(not(any(windows, unix)))]
fn duplicate_stdout() -> Option<File> {
    None
}

fn redirect_stdout_to_stderr() {
    use std::io::Write;
    let _ = std::io::stdout().flush();
    // CRT の fd 1（共有の UCRT を使うプラグインの printf はここに書く）。
    // SAFETY: fd 2 は開いたままの標準エラー。dup2 は fd 1 を差し替えるだけ。
    unsafe {
        libc::dup2(2, 1);
    }
    // OS の標準出力ハンドル（Rust の std や、後から読み込む DLL の CRT が使う）。
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Console::{
            GetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
        };
        // SAFETY: 標準エラーのハンドルを標準出力の枠に入れるだけで、閉じない。
        unsafe {
            SetStdHandle(STD_OUTPUT_HANDLE, GetStdHandle(STD_ERROR_HANDLE));
        }
    }
}
