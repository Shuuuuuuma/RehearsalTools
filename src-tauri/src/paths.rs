//! ファイルの置き場所．
//!
//! - 開発時（cargo run / debug ビルド）：リポジトリのフォルダ
//! - 本番（release ビルドの exe）：exe と同じフォルダ（設定・参照画像・周回結果・DB．exe を作り直しても消えない）

use std::path::PathBuf;
use std::sync::OnceLock;

pub fn data_root() -> &'static PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        if cfg!(debug_assertions) {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
        } else {
            std::env::current_exe()
                .ok()
                .and_then(|p| p.parent().map(|d| d.to_path_buf()))
                .unwrap_or_else(|| PathBuf::from("."))
        }
    })
}

pub fn data_dir() -> PathBuf {
    data_root().join("data")
}

pub fn config_path() -> PathBuf {
    data_root().join("automation_config.json")
}

pub fn reference_dir() -> PathBuf {
    data_root().join("references")
}

pub fn sessions_dir() -> PathBuf {
    data_root().join("sessions")
}

pub fn screenshot_dir() -> PathBuf {
    data_dir().join("hotkey_screenshots")
}
