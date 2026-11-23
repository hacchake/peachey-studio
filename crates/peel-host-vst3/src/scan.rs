//! VST3 プラグインを探す。
//!
//! アプリは [`scan_isolated`] を使う。ファイルごとに `peel-vst3-scan` を
//! 別プロセスで動かすので、読み込み中に落ちたり固まったりするプラグインが
//! あっても、そのファイルが失敗として残るだけで済む。
//!
//! 取り決め: 子プロセスはプラグインのパスを 1 つ受け取り、標準出力に
//! `PEEL-VST3-SCAN <json>` を 1 行書く。それ以外の出力は無視する。

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::info::{Vst3PluginInfo, classify};
use crate::module::{cid_hex, open};

const RESULT_PREFIX: &str = "PEEL-VST3-SCAN ";
const MAX_DEPTH: usize = 6;

/// 走査で失敗したファイル。
#[derive(Clone, Debug)]
pub struct ScanFailure {
    pub path: PathBuf,
    pub error: String,
}

#[derive(Clone, Debug, Default)]
pub struct ScanReport {
    pub plugins: Vec<Vst3PluginInfo>,
    pub failures: Vec<ScanFailure>,
}

/// VST3 を探すフォルダ。`VST3_PATH` の指定を先に、次に OS の標準の場所。
///
/// - Windows: `%COMMONPROGRAMFILES%\VST3`、`%LOCALAPPDATA%\Programs\Common\VST3`
/// - macOS: `~/Library/Audio/Plug-Ins/VST3`、`/Library/Audio/Plug-Ins/VST3`
/// - Linux: `~/.vst3`、`/usr/lib/vst3`、`/usr/local/lib/vst3`
pub fn search_paths() -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = std::env::var_os("VST3_PATH")
        .map(|v| std::env::split_paths(&v).collect())
        .unwrap_or_default();
    let env = |name: &str| std::env::var_os(name).map(PathBuf::from);
    if cfg!(windows) {
        paths.extend(env("COMMONPROGRAMFILES").map(|p| p.join("VST3")));
        paths.extend(env("LOCALAPPDATA").map(|p| p.join("Programs").join("Common").join("VST3")));
    } else if cfg!(target_os = "macos") {
        paths.extend(env("HOME").map(|p| p.join("Library/Audio/Plug-Ins/VST3")));
        paths.push("/Library/Audio/Plug-Ins/VST3".into());
    } else {
        paths.extend(env("HOME").map(|p| p.join(".vst3")));
        paths.push("/usr/lib/vst3".into());
        paths.push("/usr/local/lib/vst3".into());
    }
    let mut seen = std::collections::HashSet::new();
    paths.retain(|p| !p.as_os_str().is_empty() && seen.insert(p.clone()));
    paths
}

fn is_vst3_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("vst3"))
}

fn collect(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if is_vst3_path(&path) {
            // バンドルはフォルダだが、その中はもう探さない。
            out.push(path);
        } else if depth < MAX_DEPTH && path.is_dir() {
            collect(&path, depth + 1, out);
        }
    }
}

/// `dirs` の下の `.vst3` をすべて（整列・重複なし）。
pub fn candidate_files(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for dir in dirs {
        if is_vst3_path(dir) && dir.exists() {
            found.push(dir.clone());
        } else {
            collect(dir, 0, &mut found);
        }
    }
    found.sort();
    found.dedup();
    found
}

/// 1 つの `.vst3` の中の、音を扱うクラスをすべて読む（このプロセスで）。
pub fn scan_file(path: &Path) -> Result<Vec<Vst3PluginInfo>, String> {
    let module = open(path)?;
    Ok(module
        .classes()
        .into_iter()
        .filter(|c| c.is_audio_module())
        .map(|c| Vst3PluginInfo {
            cid: cid_hex(&c.cid),
            role: classify(&c.sub_categories),
            name: c.name,
            vendor: c.vendor,
            version: c.version,
            sub_categories: c.sub_categories,
            path: path.to_path_buf(),
        })
        .collect())
}

/// 標準の場所をこのプロセスで走査する（道具向け。アプリは [`scan_isolated`]）。
pub fn scan() -> Vec<Vst3PluginInfo> {
    let mut out = Vec::new();
    for file in candidate_files(&search_paths()) {
        match scan_file(&file) {
            Ok(mut p) => out.append(&mut p),
            Err(e) => log::warn!("VST3 scan: {e}"),
        }
    }
    out
}

#[derive(Serialize, Deserialize)]
enum WorkerMessage {
    Plugins(Vec<Vst3PluginInfo>),
    Error(String),
}

/// `peel-vst3-scan` の中身。引数のパスを 1 つ走査して結果を 1 行書く。
pub fn run_scan_worker(mut args: impl Iterator<Item = OsString>) -> ExitCode {
    let (Some(path), None) = (args.next(), args.next()) else {
        eprintln!("usage: peel-vst3-scan <file.vst3>");
        return ExitCode::from(64);
    };
    let message = match scan_file(Path::new(&path)) {
        Ok(p) => WorkerMessage::Plugins(p),
        Err(e) => WorkerMessage::Error(e),
    };
    match serde_json::to_string(&message) {
        Ok(json) => {
            println!("{RESULT_PREFIX}{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cannot encode scan result: {e}");
            ExitCode::from(70)
        }
    }
}

/// 実行ファイルの隣（テストでは 1 つ上）にある `peel-vst3-scan`。
pub fn default_scanner_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let name = format!("peel-vst3-scan{}", std::env::consts::EXE_SUFFIX);
    let dir = exe.parent()?;
    [dir.join(&name), dir.parent()?.join(&name)]
        .into_iter()
        .find(|p| p.is_file())
}

/// 1 つのファイルを子プロセスで走査する。`timeout` を過ぎたら止める。
pub fn scan_file_isolated(
    scanner: &Path,
    file: &Path,
    timeout: Duration,
) -> Result<Vec<Vst3PluginInfo>, String> {
    let mut command = Command::new(scanner);
    command
        .arg(file)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // 子プロセスのためにコンソール窓を開かない。
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("{} を起動できません: {e}", scanner.display()))?;
    let mut stdout = child.stdout.take().ok_or("標準出力を受け取れません")?;
    // おしゃべりなプラグインでパイプが詰まらないよう、別スレッドで読む。
    let reader = std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stdout.read_to_end(&mut bytes);
        bytes
    });
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{} の走査が時間切れになりました", file.display()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(e.to_string());
            }
        }
    };
    if !status.success() {
        return Err(format!(
            "{} の走査中にプラグインが落ちました（{status}）",
            file.display()
        ));
    }
    let output = reader
        .join()
        .map_err(|_| "読み取りスレッドが落ちました".to_string())?;
    let text = String::from_utf8_lossy(&output);
    let json = text
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(RESULT_PREFIX))
        .ok_or("走査の結果が出力されませんでした")?;
    match serde_json::from_str::<WorkerMessage>(json).map_err(|e| e.to_string())? {
        WorkerMessage::Plugins(p) => Ok(p),
        WorkerMessage::Error(e) => Err(e),
    }
}

/// `dirs` の下のすべての `.vst3` を、1 ファイル 1 プロセスで走査する。
pub fn scan_isolated(scanner: &Path, dirs: &[PathBuf], timeout: Duration) -> ScanReport {
    let mut report = ScanReport::default();
    for file in candidate_files(dirs) {
        match scan_file_isolated(scanner, &file, timeout) {
            Ok(mut p) => report.plugins.append(&mut p),
            Err(e) => {
                log::warn!("VST3 scan: {e}");
                report.failures.push(ScanFailure {
                    path: file,
                    error: e,
                });
            }
        }
    }
    report
}
