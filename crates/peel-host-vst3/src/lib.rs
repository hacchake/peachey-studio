//! VST3 プラグインのホスト。
//!
//! [`Vst3Device`] は VST3 プラグイン 1 つを [`peel_dsp::Device`] として包み、
//! エンジンのデバイス枠で内蔵の楽器やエフェクトと同じように動かす。
//!
//! # 使い方
//!
//! 1. **UI スレッドの外で探す。** [`scan_isolated`] を `peel-vst3-scan`
//!    （[`default_scanner_path`]）と一緒に使う。プラグインが落ちても、
//!    アプリは巻き込まれない。
//! 2. **メインスレッドで読み込む。** [`Vst3Device::load`] がプラグインを作り、
//!    記述子（型名は `vst3:<クラス ID>`、パラメータ ID はプラグイン自身の ID）
//!    を作って有効にする。
//! 3. **操作口を取ってからエンジンに渡す。** [`Vst3Device::handle`] を取り、
//!    デバイスは普通の `Box<dyn Device>` としてエンジンに送る。
//! 4. **毎フレーム [`Vst3Handle::idle`] を呼ぶ。** ホスト側で変わった値を
//!    プラグインの画面に伝え、閉じられた画面を片付ける。
//! 5. **状態は [`Vst3Handle::save_state`] で保存し、[`Vst3Handle::load_state`]
//!    で戻す。** 中身は処理側と画面側の状態をまとめたもの。
//!
//! # スレッド
//!
//! コンポーネントの準備（有効化、バスの設定）と状態の読み書き、コントローラ、
//! 画面はメインスレッド。`process` と `set_param` は音声スレッドで、確保も
//! ロックもしない。両者の値は原子変数とロックなしのキューで渡す。
//!
//! # まだないもの
//!
//! macOS と Linux での読み込みと画面、サイドチェーンや複数出力、
//! ノートエクスプレッション、プリセット（プログラム）の一覧。
#![allow(non_snake_case)]

mod com;
mod descriptor;
mod device;
mod editor;
mod handle;
mod info;
mod module;
mod plugin;
mod scan;

pub use descriptor::cached_descriptor;
pub use device::Vst3Device;
pub use handle::{IdleReport, Vst3Handle};
pub use info::{Vst3PluginInfo, Vst3Role};
pub use scan::{
    ScanFailure, ScanReport, candidate_files, default_scanner_path, run_scan_worker, scan,
    scan_file, scan_file_isolated, scan_isolated, search_paths,
};
