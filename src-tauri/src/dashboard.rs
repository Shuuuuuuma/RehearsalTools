//! 集計ダッシュボード（/）用：ウィンドウ一覧・手動キャプチャ・貼り付け画像の読み取り・自動検知スキャン．
//!
//! こちらは結果画面全体を Windows 標準の OCR で読み，「1 行にスコアが 2〜3 個並んでいる行」をステージの行とみなす．

use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use image::{imageops::FilterType, RgbImage};
use regex::Regex;
use serde_json::{json, Value};

use crate::imgproc::{contrast, resize_rgb};
use crate::storage::{ScoreStorage, StageScores};
use crate::{winapi, winocr};

const TARGET: &str = "gakumas";

/// 対象のウィンドウを完全一致 → 前方一致 → 部分一致（ブラウザや開発ツールを除く）の順で探す．
pub fn find_target_window() -> Option<winapi::Hwnd> {
    let wins = winapi::list_windows();
    let t = TARGET.to_lowercase();
    wins.iter()
        .find(|(_, n)| n.trim().to_lowercase() == t)
        .or_else(|| wins.iter().find(|(_, n)| n.trim().to_lowercase().starts_with(&t)))
        .or_else(|| {
            wins.iter().find(|(_, n)| {
                let n = n.to_lowercase();
                n.contains(&t) && !n.contains("chrome") && !n.contains("visual studio") && !n.contains("edge")
            })
        })
        .map(|(h, _)| *h)
}

/// OCR 認識結果テキストからスコア群（1 万〜1000 万）を抽出します．
pub fn extract_scores_from_text(text: &str) -> Vec<i64> {
    // +〇〇（王冠ボーナス）を安全に除外
    let filtered = Regex::new(r"\+\s*\d+").unwrap().replace_all(text, "").to_string();
    let cleaned = Regex::new(r"\s*([,\.])\s*").unwrap().replace_all(&filtered, ",").to_string();
    let in_range = |v: i64| (10_000..=10_000_000).contains(&v);

    // 3 桁区切りのカンマ付き数字
    let mut scores: Vec<i64> = Regex::new(r"\b\d{1,3}(?:,\d{3})+\b")
        .unwrap()
        .find_iter(&cleaned)
        .filter_map(|m| m.as_str().replace(',', "").parse().ok())
        .filter(|&v| in_range(v))
        .collect();
    // ピリオド区切りのフォールバック
    if scores.len() < 3 {
        for m in Regex::new(r"\b\d{1,3}(?:\.\d{3})+\b").unwrap().find_iter(&filtered) {
            if let Ok(v) = m.as_str().replace('.', "").parse::<i64>() {
                if in_range(v) && !scores.contains(&v) {
                    scores.push(v);
                }
            }
        }
    }
    // カンマなし 5〜7 桁数字のフォールバック
    if scores.len() < 3 {
        for m in Regex::new(r"\b\d{5,7}\b").unwrap().find_iter(&cleaned) {
            if let Ok(v) = m.as_str().parse::<i64>() {
                if in_range(v) && !scores.contains(&v) {
                    scores.push(v);
                }
            }
        }
    }
    scores.truncate(3);
    scores
}

fn stage_json(stage: i64, scores: &[i64]) -> Value {
    json!({"stage": stage, "scores": scores, "total_score": (!scores.is_empty()).then(|| scores.iter().sum::<i64>())})
}

/// 画像から各ステージのスコア行を検出し，総合力や合計スコアを除外して解析します．
pub fn parse_image(image: &RgbImage) -> Result<Vec<Value>, String> {
    let (w, h) = (image.width() as f64, image.height() as f64);
    let lines = winocr::recognize(image, "ja")?;

    // 1 行の中に 2〜3 個のスコア数値が含まれている行（各ステージのスコア表示行）を探す
    let mut cand: Vec<(f64, Vec<i64>)> = lines
        .iter()
        .filter_map(|l| {
            let nums = extract_scores_from_text(&l.text);
            (nums.len() >= 2).then(|| {
                let avg = if l.words.is_empty() { 0.0 } else { l.words.iter().map(|w| w.y as f64).sum::<f64>() / l.words.len() as f64 };
                (avg, nums)
            })
        })
        .collect();
    cand.sort_by(|a, b| a.0.total_cmp(&b.0));

    if cand.len() >= 3 {
        return Ok((0..3).map(|i| stage_json(i as i64 + 1, &cand[i].1)).collect());
    }
    // 3 行揃わなかった場合のフォールバック（ピンポイント領域クロップ＆拡大処理）
    let crops = [(1, 0.208, 0.252), (2, 0.448, 0.492), (3, 0.688, 0.732)];
    let mut out = Vec::new();
    for (stg, ry1, ry2) in crops {
        let (x1, y1, x2, y2) = ((w * 0.12) as u32, (h * ry1) as u32, (w * 0.88) as u32, (h * ry2) as u32);
        let near = cand.iter().find(|(cy, _)| (y1 as f64 - h * 0.05..=y2 as f64 + h * 0.05).contains(cy));
        let scores = match near {
            Some((_, nums)) => nums.clone(),
            None if x2 > x1 && y2 > y1 => {
                let strip = image::imageops::crop_imm(image, x1, y1, x2 - x1, y2 - y1).to_image();
                let scaled = contrast(&resize_rgb(&strip, strip.width() * 3, strip.height() * 3, FilterType::Lanczos3), 1.3);
                let text = winocr::recognize(&scaled, "ja")?.iter().map(|l| l.text.clone()).collect::<Vec<_>>().join(" ");
                extract_scores_from_text(&text)
            }
            None => vec![],
        };
        out.push(stage_json(stg, &scores));
    }
    Ok(out)
}

pub fn to_stage_scores(stages: &[Value]) -> Vec<StageScores> {
    stages
        .iter()
        .map(|s| StageScores {
            stage: s["stage"].as_i64().unwrap_or(0),
            scores: s["scores"].as_array().map(|a| a.iter().map(|v| v.as_i64()).collect()).unwrap_or_default(),
        })
        .collect()
}

pub fn new_trial_id() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

/// 学マスのリハーサル結果画面を常時監視し，スコアの切り替わりを自動検知して記録するスキャナー．
pub struct AutoResultScanner {
    storage: Arc<ScoreStorage>,
    capture_method: Box<dyn Fn() -> String + Send + Sync>,
    running: AtomicBool,
    message: Mutex<String>,
    detected: AtomicI64,
    last_event: Mutex<Option<f64>>,
    last_sig: Mutex<Option<Vec<i64>>>,
}

impl AutoResultScanner {
    pub fn new(storage: Arc<ScoreStorage>, capture_method: Box<dyn Fn() -> String + Send + Sync>) -> Arc<Self> {
        Arc::new(AutoResultScanner {
            storage,
            capture_method,
            running: AtomicBool::new(false),
            message: Mutex::new("待機中".into()),
            detected: AtomicI64::new(0),
            last_event: Mutex::new(None),
            last_sig: Mutex::new(None),
        })
    }

    fn say(&self, m: impl Into<String>) {
        *self.message.lock().unwrap() = m.into();
    }

    pub fn start(self: &Arc<Self>) {
        if self.running.swap(true, Ordering::SeqCst) {
            return;
        }
        self.say("監視実行中（リハーサル結果画面を待機しています）");
        let me = self.clone();
        thread::spawn(move || {
            while me.running.load(Ordering::SeqCst) {
                if let Err(e) = me.tick() {
                    me.say(format!("監視エラー: {e}"));
                }
                thread::sleep(Duration::from_millis(1500));
            }
        });
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.say("監視停止中");
    }

    fn tick(&self) -> Result<(), String> {
        let Some(h) = find_target_window() else {
            self.say("学マスウィンドウ未検出");
            return Ok(());
        };
        let Some(img) = winapi::capture_client(h, &(self.capture_method)()) else { return Ok(()) };
        let stages = parse_image(&img)?;
        let flat: Vec<i64> = stages.iter().flat_map(|s| s["scores"].as_array().cloned().unwrap_or_default()).filter_map(|v| v.as_i64()).collect();
        let valid = stages.iter().filter(|s| s["scores"].as_array().map_or(0, |a| a.len()) >= 2).count();
        // 少なくとも 2 ステージ以上（スコア計 5 個以上）が認識できている場合（リハーサル結果画面と判断）
        if valid >= 2 && flat.len() >= 5 {
            let mut sig = self.last_sig.lock().unwrap();
            if sig.as_ref() != Some(&flat) {
                // 前回のスコアと異なる（新しいリハーサル結果が出た）場合のみ自動保存
                self.storage.add_trial_scores(&new_trial_id(), &to_stage_scores(&stages)).map_err(|e| e.to_string())?;
                *sig = Some(flat.clone());
                *self.last_event.lock().unwrap() = Some(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs_f64());
                self.detected.fetch_add(1, Ordering::SeqCst);
                self.say(format!("🎉 新しいリハーサル結果を自動検知して記録しました！（検出数: {}件）", flat.len()));
            } else {
                self.say("同一の結果画面が表示されています（次のリハーサル待機中）");
            }
        } else {
            self.say("結果画面待機中（リハーサル実行中）");
        }
        Ok(())
    }

    pub fn status(&self) -> Value {
        json!({
            "is_running": self.running.load(Ordering::SeqCst),
            "status_message": *self.message.lock().unwrap(),
            "detected_count": self.detected.load(Ordering::SeqCst),
            "last_event_time": *self.last_event.lock().unwrap(),
        })
    }
}
