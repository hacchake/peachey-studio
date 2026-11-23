//! VST3 を別プロセスで調べる: `peel-vst3-scan <file.vst3>`。
//!
//! アプリはプラグインのファイルごとにこれを起動する。読み込み中に落ちたり
//! 固まったりするプラグインがあっても、アプリ本体は巻き込まれない。

use std::process::ExitCode;

fn main() -> ExitCode {
    peel_host_vst3::run_scan_worker(std::env::args_os().skip(1))
}
