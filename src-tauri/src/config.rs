//! 自動周回ツールの設定（automation_config.json）の読み書き．
//!
//! 座標はすべて「ゲームウィンドウのクライアント領域に対する比率（0.0〜1.0）」で持つ．
//! 領域は [x, y, w, h]，クリック位置は [x, y]．
//! 画面から任意の JSON を編集できるよう，Python 版と同じく JSON の値のまま扱う．

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde_json::{json, Value};

use crate::paths;

fn stage_rows(y0: f64, h: f64, x: f64, w: f64) -> Value {
    let pitch = 0.252;
    json!((0..3)
        .map(|i| {
            let y = ((y0 + pitch * i as f64) * 10000.0).round() / 10000.0;
            vec![x, y, w, h]
        })
        .collect::<Vec<_>>())
}

fn icon_grid() -> Value {
    // リハーサル開始画面（編成が並ぶ画面）の 9 枠のアイコン位置
    let xs = [0.440, 0.573, 0.707];
    let ys = [0.438, 0.574, 0.710];
    json!(ys.iter().flat_map(|&y| xs.iter().map(move |&x| vec![x, y, 0.118, 0.084])).collect::<Vec<_>>())
}

pub fn default_config() -> Value {
    json!({
        "window_title": "gakumas",
        // auto: PrintWindow（隠れていても撮れる）→ 失敗したら画面から切り出し
        "capture_method": "auto",
        // postmessage: マウスを動かさない / sendinput: 実カーソルでクリック（前面化が必要）
        "click_method": "postmessage",
        "buttons": {
            "start": {"click": [0.49, 0.847], "region": [0.36, 0.830, 0.26, 0.035], "reference": "start.png"},
            // 対戦準備画面の「スキップ」．準備中は画面が暗いので明るさでも判定する
            "skip": {"click": [0.81, 0.822], "region": [0.735, 0.81, 0.18, 0.025], "reference": "skip.png", "brightness_min": 85.0},
            // 空き枠がある編成で出る「メモリー未編成」確認ダイアログの「開始する」
            "confirm": {
                "click": [0.69, 0.905],
                "region": [0.60, 0.89, 0.22, 0.03],
                "cancel_region": [0.15, 0.89, 0.28, 0.03],
                "reference": "confirm.png"
            },
            "end": {"click": [0.50, 0.905], "region": [0.40, 0.886, 0.20, 0.038], "reference": "end.png"}
        },
        // 対戦準備画面右下の「コンテスト準備中...」（白文字）．これが出ている間はスキップを押さずに待つ
        "preparing": {"region": [0.70, 0.912, 0.25, 0.03], "white_min": 0.05},
        "match_threshold": 0.85,
        "confirm_count": 3,
        "poll_interval_ms": 250,
        "page_timeout_ms": 30000,
        "loading_timeout_ms": 60000,
        "prepare_timeout_ms": 0,
        "result_timeout_ms": 30000,
        "capture_delay_ms": 500,
        "max_click_retries": 5,
        "click_retry_interval_ms": 1200,
        "ocr": {
            "engine": "auto",
            "lang": "ja",
            "score_threshold": 190,
            "total_threshold": 210,
            "bonus_blue_min": 190,
            "bonus_margin": 30,
            "scale": 3
        },
        "regions": {
            "score": stage_rows(0.178, 0.022, 0.28, 0.44),
            // 3 人分の列の境界（クライアント比率の x）
            "score_columns": [0.28, 0.433, 0.577, 0.72],
            "total": stage_rows(0.137, 0.036, 0.30, 0.40),
            // 1 位のアイドルの上に出る吹き出し．1 位の列で位置が変わるので横幅いっぱい
            "bonus": stage_rows(0.200, 0.024, 0.28, 0.44),
            "icons": icon_grid()
        },
        "lineup_threshold": 20.0
    })
}

pub fn deep_merge(base: &Value, patch: &Value) -> Value {
    let mut out = base.clone();
    if let (Some(o), Some(p)) = (out.as_object_mut(), patch.as_object()) {
        for (k, v) in p {
            let merged = match o.get(k) {
                Some(cur) if cur.is_object() && v.is_object() => deep_merge(cur, v),
                _ => v.clone(),
            };
            o.insert(k.clone(), merged);
        }
    }
    out
}

pub struct ConfigStore {
    path: PathBuf,
    data: Mutex<Value>,
}

impl ConfigStore {
    pub fn new() -> Self {
        let path = paths::config_path();
        let _ = fs::create_dir_all(paths::reference_dir());
        let _ = fs::create_dir_all(paths::sessions_dir());
        let loaded = fs::read_to_string(&path)
            .ok()
            .map(|s| serde_json::from_str::<Value>(s.trim_start_matches('\u{feff}')));
        let data = match loaded {
            Some(Ok(v)) => deep_merge(&default_config(), &v),
            other => {
                if let Some(Err(e)) = other {
                    eprintln!("[config] 読み込み失敗のため既定値を使用: {e}");
                }
                let d = default_config();
                write(&path, &d);
                d
            }
        };
        ConfigStore { path, data: Mutex::new(data) }
    }

    pub fn get(&self) -> Value {
        self.data.lock().unwrap().clone()
    }

    pub fn update(&self, patch: &Value) -> Value {
        let mut d = self.data.lock().unwrap();
        *d = deep_merge(&d, patch);
        write(&self.path, &d);
        d.clone()
    }

    pub fn reset(&self) -> Value {
        let mut d = self.data.lock().unwrap();
        *d = default_config();
        write(&self.path, &d);
        d.clone()
    }

    pub fn reference_path(name: &str) -> PathBuf {
        paths::reference_dir().join(name)
    }
}

fn write(path: &PathBuf, data: &Value) {
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(data).unwrap_or_default();
    if fs::write(&tmp, text).is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

// ---- JSON から値を取り出す小道具 ----
pub fn f(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

pub fn region(v: &Value) -> [f64; 4] {
    [f(&v[0]), f(&v[1]), f(&v[2]), f(&v[3])]
}

pub fn point(v: &Value) -> (f64, f64) {
    (f(&v[0]), f(&v[1]))
}

pub fn regions(v: &Value) -> Vec<[f64; 4]> {
    v.as_array().map(|a| a.iter().map(region).collect()).unwrap_or_default()
}

pub fn s<'a>(v: &'a Value) -> &'a str {
    v.as_str().unwrap_or("")
}
