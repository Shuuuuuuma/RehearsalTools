//! Python 版との突き合わせ用（保存済みスクショを読んで JSON Lines に書き出す）．普段は実行されない．
//!
//!   CHECK_LIST=画像パスの一覧 CHECK_OUT=出力先 cargo test -- --ignored ocr_check
//!   CHECK_LIST=画像パスの一覧 CHECK_OUT=出力先 cargo test -- --ignored detect_check

use std::io::Write;
use std::sync::Arc;

use serde_json::json;

use crate::config::ConfigStore;
use crate::detector::ScreenDetector;
use crate::ocr::{OcrEngine, ResultReader};

fn list() -> (Vec<String>, std::fs::File) {
    let paths = std::fs::read_to_string(std::env::var("CHECK_LIST").unwrap()).unwrap();
    let out = std::fs::File::create(std::env::var("CHECK_OUT").unwrap()).unwrap();
    (paths.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect(), out)
}

#[test]
#[ignore]
fn ocr_check() {
    let cfg = ConfigStore::new().get();
    let mut reader = ResultReader::new(OcrEngine::new("template", "ja").unwrap());
    reader.learn = false;
    let (paths, mut out) = list();
    for p in paths {
        let img = image::open(&p).unwrap().to_rgb8();
        writeln!(out, "{}", json!({"path": p, "result": reader.read(&img, &cfg)})).unwrap();
    }
}

#[test]
#[ignore]
fn winocr_check() {
    let (paths, mut out) = list();
    for p in paths {
        let img = image::open(&p).unwrap().to_rgb8();
        writeln!(out, "{}", json!({"path": p, "result": crate::dashboard::parse_image(&img).unwrap()})).unwrap();
    }
}

#[test]
#[ignore]
fn detect_check() {
    let det = ScreenDetector::new(Arc::new(ConfigStore::new()));
    let (paths, mut out) = list();
    for p in paths {
        let img = image::open(&p).unwrap().to_rgb8();
        writeln!(out, "{}", json!({"path": p, "result": det.check_all(&img)})).unwrap();
    }
}
