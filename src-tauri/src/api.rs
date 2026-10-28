//! 画面から呼ばれる API（/api/...）．
//!
//! Python 版の FastAPI と同じ URL・JSON にしてあるので，画面の HTML/JS はほぼそのまま使える．
//! 画面とは Tauri の独自プロトコル（app://）でやりとりする（main.rs）．

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use base64::Engine;
use image::RgbImage;
use regex::Regex;
use serde_json::{json, Value};

use crate::config::{point, regions, s, ConfigStore};
use crate::dashboard::{self, AutoResultScanner};
use crate::detector::{lineup_distance, visible};
use crate::imgproc::crop_rel;
use crate::ocr::ResultReader;
use crate::runner::AutomationRunner;
use crate::session::{self, value_cols, Row, Session, SCORE_COLS};
use crate::storage::ScoreStorage;
use crate::{paths, winapi};

/// 画面側の表示と合わせる版．
pub const VERSION: &str = "2026-10-28.1";

pub struct Resp {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

type R = Result<Resp, (u16, String)>;

fn ok(v: Value) -> R {
    Ok(Resp { status: 200, content_type: "application/json", body: serde_json::to_vec(&v).unwrap_or_default() })
}

fn png(img: &RgbImage) -> R {
    let mut buf = Cursor::new(Vec::new());
    img.write_to(&mut buf, image::ImageFormat::Png).map_err(|e| (500, e.to_string()))?;
    Ok(Resp { status: 200, content_type: "image/png", body: buf.into_inner() })
}

fn file(path: &PathBuf) -> R {
    let body = fs::read(path).map_err(|_| (404, "見つかりません".to_string()))?;
    Ok(Resp { status: 200, content_type: "image/png", body })
}

fn bad(msg: impl Into<String>) -> (u16, String) {
    (400, msg.into())
}

fn not_found() -> (u16, String) {
    (404, "見つかりません".into())
}

pub fn error_response(code: u16, msg: &str) -> Resp {
    Resp { status: code, content_type: "application/json", body: serde_json::to_vec(&json!({"detail": msg})).unwrap_or_default() }
}

fn decode_image(data: &str) -> Result<RgbImage, (u16, String)> {
    let data = data.split_once(',').map(|(_, d)| d).unwrap_or(data);
    let bytes = base64::engine::general_purpose::STANDARD.decode(data.trim()).map_err(|e| bad(format!("画像のデコードに失敗しました: {e}")))?;
    Ok(image::load_from_memory(&bytes).map_err(|e| bad(format!("画像のデコードに失敗しました: {e}")))?.to_rgb8())
}

fn open_session(sid: &str) -> Result<Session, (u16, String)> {
    if !session::valid_id(sid) {
        return Err(bad("不正なセッション ID"));
    }
    Session::open(sid).ok_or((404, "セッションが見つかりません".into()))
}

/// 値を「空欄 → None，"1,234" → 1234」として読む．
fn cell_value(v: &Value) -> Result<Option<i64>, (u16, String)> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(n.as_i64().or_else(|| n.as_f64().map(|f| f as i64))),
        Value::String(t) if t.trim().is_empty() => Ok(None),
        Value::String(t) => t.trim().replace(',', "").parse().map(Some).map_err(|_| bad(format!("数値ではありません: {t}"))),
        _ => Err(bad("数値ではありません")),
    }
}

pub struct App {
    pub config: Arc<ConfigStore>,
    pub runner: Arc<AutomationRunner>,
    pub storage: Arc<ScoreStorage>,
    pub scanner: Arc<AutoResultScanner>,
    pub hotkeys: Arc<Mutex<Vec<String>>>,
    pub window: OnceLock<tauri::WebviewWindow>,
    lineup_screen: Mutex<Option<RgbImage>>,
}

impl App {
    pub fn new(
        config: Arc<ConfigStore>,
        runner: Arc<AutomationRunner>,
        storage: Arc<ScoreStorage>,
        scanner: Arc<AutoResultScanner>,
        hotkeys: Arc<Mutex<Vec<String>>>,
    ) -> Self {
        App { config, runner, storage, scanner, hotkeys, window: OnceLock::new(), lineup_screen: Mutex::new(None) }
    }

    fn hwnd(&self) -> Result<winapi::Hwnd, (u16, String)> {
        winapi::find_window(s(&self.config.get()["window_title"])).ok_or((404, "ゲームウィンドウが見つかりません".into()))
    }

    fn screen(&self) -> Result<RgbImage, (u16, String)> {
        winapi::capture_client(self.hwnd()?, s(&self.config.get()["capture_method"]))
            .ok_or((500, "キャプチャに失敗しました（最小化されていませんか？）".into()))
    }

    fn reader(&self) -> Result<Arc<ResultReader>, (u16, String)> {
        self.runner.reader().map_err(|e| (500, e))
    }

    /// 保存ダイアログを出してファイルに書く．
    fn save_dialog(&self, name: &str, data: &[u8]) -> R {
        let ext = name.rsplit('.').next().unwrap_or("csv").to_string();
        let mut dlg = rfd::FileDialog::new().set_file_name(name).add_filter(ext.to_uppercase(), &[ext.as_str()]);
        if let Some(w) = self.window.get() {
            dlg = dlg.set_parent(w);
        }
        match dlg.save_file() {
            Some(p) => {
                fs::write(&p, data).map_err(|e| (500, format!("保存に失敗しました: {e}")))?;
                ok(json!({"saved": p.to_string_lossy()}))
            }
            None => ok(json!({"saved": null})),
        }
    }

    pub fn handle(&self, method: &str, path: &str, query: &HashMap<String, String>, body: &[u8]) -> R {
        let body: Value = if body.is_empty() { json!({}) } else { serde_json::from_slice(body).map_err(|e| bad(format!("JSON が不正です: {e}")))? };
        let seg: Vec<&str> = path.trim_matches('/').split('/').collect();
        match (method, &seg[..]) {
            // ================= 自動周回 =================
            ("GET", ["api", "auto", "env"]) => self.env(),
            ("GET", ["api", "auto", "status"]) => ok(self.runner.get_status()),
            ("POST", ["api", "auto", "start"]) => {
                let n = body["iterations"].as_i64().ok_or_else(|| bad("周回数が必要です"))?;
                let sid = body["session_id"].as_str().filter(|s| !s.is_empty()).map(String::from);
                self.runner.start(n, sid).map(Value::from).map_err(bad).and_then(ok)
            }
            ("POST", ["api", "auto", "lineup_check"]) => self.lineup_check(s(&body["session_id"])),
            ("GET", ["api", "auto", "lineup_current", idx]) => {
                let idx: usize = idx.parse().map_err(|_| not_found())?;
                let screen = self.lineup_screen.lock().unwrap().clone().ok_or_else(not_found)?;
                let icons = regions(&self.config.get()["regions"]["icons"]);
                png(&crop_rel(&screen, *icons.get(idx).ok_or_else(not_found)?))
            }
            ("POST", ["api", "auto", "abort"]) => {
                self.runner.abort();
                ok(json!({"ok": true}))
            }
            ("POST", ["api", "auto", "screenshot"]) => self.runner.screenshot().map_err(bad).and_then(ok),

            // ---------------- キャリブレーション ----------------
            ("GET", ["api", "auto", "preview"]) => png(&self.screen()?),
            ("GET", ["api", "auto", "detect"]) => ok(Value::Object(self.runner.detector.check_all(&self.screen()?))),
            ("POST", ["api", "auto", "reference", button]) => {
                if !["start", "skip", "end", "confirm"].contains(button) {
                    return Err(bad("button は start / skip / end / confirm"));
                }
                let p = self.runner.detector.save_reference(button, &self.screen()?).map_err(|e| (500, e))?;
                ok(json!({"ok": true, "path": p.to_string_lossy()}))
            }
            ("GET", ["api", "auto", "reference", button]) => {
                if !["start", "skip", "end", "confirm"].contains(button) {
                    return Err(bad("button は start / skip / end / confirm"));
                }
                file(&ConfigStore::reference_path(s(&self.config.get()["buttons"][*button]["reference"])))
            }
            ("POST", ["api", "auto", "click_test"]) => {
                if self.runner.running() {
                    return Err(bad("自動周回中はテストできません"));
                }
                let h = self.hwnd()?;
                let cfg = self.config.get();
                let (x, y) = match body["button"].as_str() {
                    Some(b) => point(&cfg["buttons"][b]["click"]),
                    None => (body["x"].as_f64().unwrap_or(0.0), body["y"].as_f64().unwrap_or(0.0)),
                };
                winapi::click(h, x, y, s(&cfg["click_method"]));
                ok(json!({"ok": true, "x": x, "y": y, "method": cfg["click_method"]}))
            }
            ("POST", ["api", "auto", "ocr_test"]) => {
                let img = match body["image_base64"].as_str().filter(|s| !s.is_empty()) {
                    Some(d) => decode_image(d)?,
                    None => self.screen()?,
                };
                ok(self.reader()?.read(&img, &self.config.get()))
            }

            // ---------------- 設定 ----------------
            ("GET", ["api", "auto", "config"]) => ok(self.config.get()),
            ("POST", ["api", "auto", "config"]) => {
                if self.runner.running() {
                    return Err(bad("自動周回中は設定を変更できません"));
                }
                let cfg = self.config.update(&body);
                self.runner.reset_reader(); // OCR 設定を反映
                ok(cfg)
            }
            ("POST", ["api", "auto", "config", "reset"]) => ok(self.config.reset()),

            // ---------------- セッション ----------------
            ("GET", ["api", "auto", "sessions"]) => ok(json!({"sessions": Session::list_all()})),
            ("GET", ["api", "auto", "sessions", sid, "rows"]) => {
                ok(json!({"rows": open_session(sid)?.rows().iter().map(Row::to_json).collect::<Vec<_>>()}))
            }
            ("POST", ["api", "auto", "sessions", sid, "rows"]) => self.save_rows(sid, &body),
            ("GET", ["api", "auto", "sessions", sid, "stats"]) => ok(open_session(sid)?.statistics()),
            ("POST", ["api", "auto", "sessions", sid, "verify", it]) => {
                let it: i64 = it.parse().map_err(|_| bad("不正な番号"))?;
                let s = open_session(sid)?;
                let mut rows = s.rows();
                rows.iter_mut().filter(|r| r.iteration == it).for_each(|r| r.status = "verified".into());
                s.save_rows(&mut rows).map_err(|e| (500, e.to_string()))?;
                ok(json!({"ok": true}))
            }
            ("POST", ["api", "auto", "sessions", sid, "reocr"]) => self.reocr(sid),
            ("POST", ["api", "auto", "merge_suggest"]) => {
                let (base, source) = self.merge_pair(&body)?;
                ok(session::suggest_mapping(&base, &source, &regions(&self.config.get()["regions"]["icons"])))
            }
            ("POST", ["api", "auto", "merge"]) => {
                let (base, source) = self.merge_pair(&body)?;
                let mapping: Vec<Option<usize>> = body["mapping"]
                    .as_array()
                    .filter(|a| a.len() == 9)
                    .ok_or_else(|| bad("対応付け（9 枠分）が必要です"))?
                    .iter()
                    .map(|v| match v {
                        Value::Null => Ok(None),
                        v => v.as_u64().filter(|&m| m < 9).map(|m| Some(m as usize)).ok_or_else(|| bad("対応付けの値が不正です")),
                    })
                    .collect::<Result<_, _>>()?;
                if mapping.iter().all(|m| m.is_none()) {
                    return Err(bad("足す枠が 1 つも選ばれていません"));
                }
                let new = session::merge_sessions(&base, &source, &mapping).map_err(|e| (500, e.to_string()))?;
                ok(json!({"session": new.id, "rows": new.rows().len()}))
            }
            ("POST", ["api", "auto", "sessions", sid, "charts"]) => {
                // グラフは画面側で描く．描画に使う統計と行を返す
                let s = open_session(sid)?;
                let stats = s.write_statistics();
                ok(json!({"stats": stats, "rows": s.rows().iter().map(Row::to_json).collect::<Vec<_>>()}))
            }
            ("GET", ["api", "auto", "sessions", sid, "download", kind]) => {
                let s = open_session(sid)?;
                let (data, name) = match *kind {
                    "results" => (fs::read(&s.results_csv).ok(), format!("{sid}_results.csv")),
                    "raw" => (fs::read(&s.raw_csv).ok(), format!("{sid}_rehearsal_data.csv")),
                    "stats" => {
                        let text = s.statistics_csv();
                        let _ = fs::write(s.dir.join("statistics.csv"), &text);
                        (Some(text.into_bytes()), format!("{sid}_statistics.csv"))
                    }
                    _ => return Err(bad("kind は results / raw / stats")),
                };
                self.save_dialog(&name, &data.ok_or((404, "まだデータがありません".to_string()))?)
            }
            ("GET", ["api", "auto", "sessions", sid, "screenshot", it]) => {
                file(&open_session(sid)?.screenshot_path(it.parse().map_err(|_| not_found())?))
            }
            ("POST", ["api", "auto", "sessions", sid, "screenshot", it, "open"]) => {
                let p = open_session(sid)?.screenshot_path(it.parse().map_err(|_| not_found())?);
                if !p.exists() {
                    return Err(not_found());
                }
                winapi::open_path(&p.to_string_lossy());
                ok(json!({"ok": true}))
            }
            ("GET", ["api", "auto", "sessions", sid, "crop", it, stage]) => {
                let p = open_session(sid)?.screenshot_path(it.parse().map_err(|_| not_found())?);
                let stage: usize = stage.parse().map_err(|_| not_found())?;
                if !(1..=3).contains(&stage) {
                    return Err(not_found());
                }
                let img = image::open(&p).map_err(|_| not_found())?.to_rgb8();
                png(&ResultReader::stage_crop(&img, &self.config.get(), stage - 1))
            }
            ("GET", ["api", "auto", "sessions", sid, "icon", idx]) => {
                let s = open_session(sid)?;
                let idx: usize = idx.parse().map_err(|_| not_found())?;
                let icons = regions(&self.config.get()["regions"]["icons"]);
                let img = s.load_start_page().ok_or_else(not_found)?;
                png(&crop_rel(&img, *icons.get(idx).ok_or_else(not_found)?))
            }
            ("POST", ["api", "auto", "sessions", sid, "open"]) => {
                let s = open_session(sid)?;
                winapi::open_path(&s.dir.to_string_lossy());
                ok(json!({"path": s.dir.to_string_lossy()}))
            }

            // ================= 集計ダッシュボード =================
            ("GET", ["api", "windows"]) => {
                let wins: Vec<Value> = winapi::list_windows().into_iter().map(|(h, t)| json!({"hwnd": h, "title": t})).collect();
                ok(json!({"target_hwnd": dashboard::find_target_window(), "windows": wins}))
            }
            ("POST", ["api", "capture"]) => {
                let h = body["hwnd"].as_i64().map(|h| h as winapi::Hwnd).or_else(dashboard::find_target_window).ok_or((
                    404,
                    "ゲームウィンドウ（gakumas）が見つかりません．ゲームが起動しているか確認してください．".to_string(),
                ))?;
                let img = winapi::capture_client(h, s(&self.config.get()["capture_method"])).ok_or((
                    500,
                    "ウィンドウのキャプチャに失敗しました．ウィンドウが最小化されていないか確認してください．".to_string(),
                ))?;
                self.register_image(&img, "スコアが検出できませんでした．学マスのゲーム画面（リハーサル結果）が手前に表示されているか確認してください．", true)
            }
            ("POST", ["api", "upload_image"]) => {
                let img = decode_image(s(&body["image_base64"]))?;
                self.register_image(&img, "画像からスコアが検出できませんでした．リハーサル結果画面のスクリーンショットを貼り付けてください．", true)
            }
            ("POST", ["api", "capture_sample"]) => {
                // data フォルダに sample_result.png（リハーサル結果画面のスクショ）を置くと読み込める
                let img = image::open(paths::data_dir().join("sample_result.png"))
                    .map_err(|_| (404, "サンプル画像が見つかりません．".to_string()))?
                    .to_rgb8();
                self.register_image(&img, "", false)
            }
            ("POST", ["api", "auto_scan", "start"]) => {
                self.scanner.start();
                ok(json!({"success": true, "status": self.scanner.status()}))
            }
            ("POST", ["api", "auto_scan", "stop"]) => {
                self.scanner.stop();
                ok(json!({"success": true, "status": self.scanner.status()}))
            }
            ("GET", ["api", "auto_scan", "status"]) => ok(json!({"status": self.scanner.status()})),
            ("GET", ["api", "last_capture_image"]) => file(&paths::data_dir().join("last_capture.png")),
            ("POST", ["api", "set_idol_name"]) => {
                let (stage, slot) = (body["stage"].as_i64().unwrap_or(0), body["slot"].as_i64().unwrap_or(0));
                self.storage.set_idol_name(stage, slot, s(&body["name"]).trim()).map_err(|e| (500, e.to_string()))?;
                ok(json!({"success": true, "idol_names": self.storage.idol_names()}))
            }
            ("GET", ["api", "trials"]) => ok(json!({"trials": self.storage.recent_trials(50)})),
            ("GET", ["api", "rehearsal_rows"]) => self.rehearsal_rows(query.get("source").map(String::as_str).unwrap_or("db")),
            ("GET", ["api", "export_csv"]) => {
                let name = format!("rehearsal_scores_{}.csv", chrono::Local::now().format("%Y%m%d_%H%M%S"));
                self.save_dialog(&name, self.storage.export_csv().as_bytes())
            }
            ("POST", ["api", "reset"]) => {
                self.storage.clear_all().map_err(|e| (500, e.to_string()))?;
                ok(json!({"success": true}))
            }

            // ================= 画面用の小道具 =================
            ("POST", ["api", "save_text"]) => self.save_dialog(s(&body["name"]), s(&body["content"]).as_bytes()),
            ("POST", ["api", "open_url"]) => {
                let url = s(&body["url"]);
                if !url.starts_with("https://") {
                    return Err(bad("https の URL のみ開けます"));
                }
                winapi::open_path(url);
                ok(json!({"ok": true}))
            }
            _ => Err(not_found()),
        }
    }

    fn env(&self) -> R {
        let cfg = self.config.get();
        let h = winapi::find_window(s(&cfg["window_title"]));
        let engine = match self.runner.reader() {
            Ok(r) => r.engine.name.to_string(),
            Err(e) => format!("使用不可: {e}"),
        };
        let refs: serde_json::Map<String, Value> =
            ["start", "skip", "end"].iter().map(|b| (b.to_string(), json!(self.runner.detector.has_reference(b)))).collect();
        ok(json!({
            "version": VERSION,
            "is_windows": true,
            "is_admin": winapi::is_admin(),
            "window_found": h.is_some(),
            "client_size": h.map(|h| { let (w, ht) = winapi::client_size(h); vec![w, ht] }),
            "ocr_engine": engine,
            "references": refs,
            "hotkeys": *self.hotkeys.lock().unwrap(),
            "last_iterations": self.runner.last_iterations.load(std::sync::atomic::Ordering::SeqCst),
        }))
    }

    /// 追加周回の前に，今の編成がセッション開始時と同じかを確認する．
    fn lineup_check(&self, sid: &str) -> R {
        let s = open_session(sid)?;
        let Some(prev) = s.load_start_page() else {
            return ok(json!({"has_reference": false, "changed": []}));
        };
        let screen = self.screen()?;
        if !visible(&self.runner.detector.check_all(&screen), "start") {
            return Err(bad("ゲームをリハーサル画面（編成が並んでいる画面）にしてから開始してください"));
        }
        let cfg = self.config.get();
        let dist = lineup_distance(&screen, &prev, &regions(&cfg["regions"]["icons"]));
        let thr = cfg["lineup_threshold"].as_f64().unwrap_or(20.0);
        *self.lineup_screen.lock().unwrap() = Some(screen);
        ok(json!({
            "has_reference": true,
            "threshold": thr,
            "distances": dist.iter().map(|d| (d * 10.0).round() / 10.0).collect::<Vec<_>>(),
            "changed": dist.iter().enumerate().filter(|(_, &d)| d > thr).map(|(i, _)| i).collect::<Vec<_>>(),
        }))
    }

    fn save_rows(&self, sid: &str, body: &Value) -> R {
        let s = open_session(sid)?;
        let mut old = s.rows();
        let mut changed = 0;
        for row in body["rows"].as_array().cloned().unwrap_or_default() {
            let it = match &row["iteration"] {
                Value::String(t) => t.parse().ok(),
                v => v.as_i64(),
            };
            let Some(base) = it.and_then(|it| old.iter_mut().find(|r| r.iteration == it)) else { continue };
            let mut edits = false;
            for k in value_cols() {
                if let Some(v) = row.get(k) {
                    let v = cell_value(v)?;
                    if v != base.get(k) {
                        base.set(k, v);
                        edits = true;
                    }
                }
            }
            if edits {
                base.status = "manual".into();
                base.recovery = "manual".into();
                changed += 1;
            } else if row["status"] == json!("verified") && base.status != "verified" {
                base.status = "verified".into();
                changed += 1;
            }
        }
        s.save_rows(&mut old).map_err(|e| (500, e.to_string()))?;
        s.write_statistics();
        ok(json!({"ok": true, "changed": changed}))
    }

    /// 保存済みスクショを今の読み取り方式で読み直す（手修正した行はそのまま）．
    fn reocr(&self, sid: &str) -> R {
        if self.runner.running() {
            return Err(bad("自動周回中は実行できません"));
        }
        let s = open_session(sid)?;
        let cfg = self.config.get();
        let reader = self.reader()?;
        let old: HashMap<i64, Row> = s.rows().into_iter().map(|r| (r.iteration, r)).collect();
        let mut counts = serde_json::Map::new();
        let mut bump = |k: &str| {
            let n = counts.get(k).and_then(|v| v.as_i64()).unwrap_or(0);
            counts.insert(k.to_string(), json!(n + 1));
        };
        let mut rows = Vec::new();
        for it in s.screenshot_iterations() {
            let prev = old.get(&it);
            if let Some(p) = prev.filter(|p| p.recovery == "manual") {
                rows.push(p.clone());
                bump("manual");
                continue;
            }
            let Ok(img) = image::open(s.screenshot_path(it)) else { continue };
            let res = reader.read(&img.to_rgb8(), &cfg);
            rows.push(Row::from_result(it, prev.map(|p| p.timestamp.clone()).unwrap_or_default(), &res, "auto"));
            bump(s_str(&res["status"]));
        }
        s.save_rows(&mut rows).map_err(|e| (500, e.to_string()))?;
        s.write_statistics();
        ok(json!({"ok": true, "counts": counts, "iterations": rows.len()}))
    }

    fn merge_pair(&self, body: &Value) -> Result<(Session, Session), (u16, String)> {
        let (base, source) = (open_session(s(&body["base"]))?, open_session(s(&body["source"]))?);
        if base.id == source.id {
            return Err(bad("結合先と結合元が同じです"));
        }
        Ok((base, source))
    }

    /// キャプチャ・貼り付け画像を OCR してダッシュボードに登録する．
    fn register_image(&self, img: &RgbImage, empty_msg: &str, require_scores: bool) -> R {
        let _ = fs::create_dir_all(paths::data_dir());
        let _ = img.save(paths::data_dir().join("last_capture.png"));
        let stages = dashboard::parse_image(img).map_err(|e| (500, e))?;
        let total: usize = stages.iter().map(|s| s["scores"].as_array().map_or(0, |a| a.len())).sum();
        if require_scores && total == 0 {
            return Err(bad(empty_msg));
        }
        let trial_id = dashboard::new_trial_id();
        self.storage.add_trial_scores(&trial_id, &dashboard::to_stage_scores(&stages)).map_err(|e| (500, e.to_string()))?;
        ok(json!({"success": true, "trial_id": trial_id, "detected_count": total, "stages": stages}))
    }

    /// リハーサル分析カード用：1 回 = 3 ステージ × 3 人のスコア表．
    fn rehearsal_rows(&self, source: &str) -> R {
        let names = self.storage.idol_names();
        let rows: Vec<Value> = if let Some(sid) = source.strip_prefix("session:") {
            let s = open_session(sid)?;
            s.rows()
                .iter()
                .map(|r| {
                    let flags: Vec<&str> = (1..=3)
                        .map(|st| {
                            let m = Regex::new(&format!(r"S{st}:([^;]*)")).unwrap().captures(&r.flags).map(|c| c[1].to_string());
                            let bad = r.status == "flagged"
                                && m.is_none_or(|m| ["mismatch", "ambiguous", "error"].iter().any(|k| m.contains(k)));
                            if bad { "flagged" } else { "ok" }
                        })
                        .collect();
                    let scores: Vec<Vec<i64>> =
                        (0..3).map(|st| (0..3).map(|c| r.get(SCORE_COLS[st * 3 + c]).unwrap_or(0)).collect()).collect();
                    json!({"iteration": r.iteration, "flags": flags, "scores": scores})
                })
                .collect()
        } else {
            self.storage
                .trial_grid()
                .into_iter()
                .map(|(t, g)| json!({"trial_id": t, "flags": ["ok", "ok", "ok"], "scores": g}))
                .collect()
        };
        ok(json!({"rows": rows, "idol_names": names}))
    }
}

fn s_str(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}
