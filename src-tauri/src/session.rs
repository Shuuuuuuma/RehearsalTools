//! 周回セッションの保存：スクショ・CSV・統計．
//!
//! sessions/<YYYYmmdd_HHMMSS>/
//!   start_page.png        1 周目の開始前に撮ったリハーサル画面（編成チェック・グラフのラベル用）
//!   screenshots/0001.png  各周の結果画面
//!   results.csv           周ごとの OCR 結果（ヘッダ付き，状態・フラグ付き）
//!   rehearsal_data.csv    9 人分のスコアのみ（ヘッダなし，表計算に貼り付け用）
//!   statistics.json       列ごとの統計
//!
//! グラフは画面側（JS）で描く．

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use chrono::Local;
use image::RgbImage;
use serde_json::{json, Map, Value};

use crate::detector::{icon_distance, icon_small};
use crate::paths;

pub const SCORE_COLS: [&str; 9] = ["S1C1", "S1C2", "S1C3", "S2C1", "S2C2", "S2C3", "S3C1", "S3C2", "S3C3"];
pub const TOTAL_COLS: [&str; 3] = ["S1_total", "S2_total", "S3_total"];
pub const BONUS_COLS: [&str; 3] = ["S1_bonus", "S2_bonus", "S3_bonus"];
/// 数値の列（スコア 9・合計 3・ボーナス 3）
pub fn value_cols() -> impl Iterator<Item = &'static str> {
    SCORE_COLS.iter().chain(TOTAL_COLS.iter()).chain(BONUS_COLS.iter()).copied()
}

fn header() -> Vec<&'static str> {
    let mut h = vec!["iteration", "timestamp"];
    h.extend(value_cols());
    h.extend(["status", "flags", "recovery"]);
    h
}

// ファイルの書き込みはまとめて 1 つのロックで守る（周回中の追記とレビューの保存が重ならないように）
static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug)]
pub struct Row {
    pub iteration: i64,
    pub timestamp: String,
    pub status: String,
    pub flags: String,
    pub recovery: String,
    /// value_cols() の順
    pub vals: [Option<i64>; 15],
}

pub fn col_index(k: &str) -> Option<usize> {
    value_cols().position(|c| c == k)
}

impl Row {
    pub fn get(&self, k: &str) -> Option<i64> {
        col_index(k).and_then(|i| self.vals[i])
    }

    pub fn set(&mut self, k: &str, v: Option<i64>) {
        if let Some(i) = col_index(k) {
            self.vals[i] = v;
        }
    }

    pub fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("iteration".into(), json!(self.iteration));
        m.insert("timestamp".into(), json!(self.timestamp));
        m.insert("status".into(), json!(self.status));
        m.insert("flags".into(), json!(self.flags));
        m.insert("recovery".into(), json!(self.recovery));
        for (k, v) in value_cols().zip(self.vals) {
            m.insert(k.into(), json!(v));
        }
        Value::Object(m)
    }

    fn csv_fields(&self) -> Vec<String> {
        let mut out = vec![self.iteration.to_string(), self.timestamp.clone()];
        out.extend(self.vals.iter().map(|v| v.map(|x| x.to_string()).unwrap_or_default()));
        out.extend([self.status.clone(), self.flags.clone(), self.recovery.clone()]);
        out
    }

    /// OCR の結果から行を作る．
    pub fn from_result(iteration: i64, timestamp: String, res: &Value, recovery: &str) -> Row {
        let mut vals = [None; 15];
        for st in 0..3 {
            let stage = &res["stages"][st];
            for c in 0..3 {
                vals[st * 3 + c] = stage["scores"][c].as_i64();
            }
            vals[9 + st] = stage["total"].as_i64();
            vals[12 + st] = stage["bonus"].as_i64();
        }
        Row {
            iteration,
            timestamp,
            status: res["status"].as_str().unwrap_or("flagged").to_string(),
            flags: res["flags"].as_str().unwrap_or("").to_string(),
            recovery: recovery.to_string(),
            vals,
        }
    }
}

/// "1,234" も数値として読む（空欄・読めないものは None）．
pub fn to_int(v: &str) -> Option<i64> {
    let t = v.trim();
    if t.is_empty() || t == "None" {
        return None;
    }
    t.replace(',', "").parse().ok()
}

pub fn valid_id(sid: &str) -> bool {
    !sid.is_empty() && sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

// --------------------------------------------------------------------------
// 統計
// --------------------------------------------------------------------------
/// numpy.percentile（線形補間）．
pub fn percentile(sorted: &[f64], p: f64) -> f64 {
    let i = (sorted.len() - 1) as f64 * p / 100.0;
    let (lo, hi) = (i.floor() as usize, i.ceil() as usize);
    sorted[lo] + (sorted[hi] - sorted[lo]) * (i - lo as f64)
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round_ties_even() / 10.0
}

pub fn describe(values: &[i64]) -> Value {
    if values.is_empty() {
        return json!({"n": 0, "avg": null, "median": null, "max": null, "min": null, "q1": null, "q3": null, "std": null});
    }
    let mut a: Vec<f64> = values.iter().map(|&v| v as f64).collect();
    a.sort_by(|x, y| x.total_cmp(y));
    let n = a.len() as f64;
    let mean = a.iter().sum::<f64>() / n;
    let std = if a.len() > 1 { round1((a.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt()) } else { 0.0 };
    json!({
        "n": a.len(),
        "avg": round1(mean),
        "median": percentile(&a, 50.0),
        "max": *values.iter().max().unwrap(),
        "min": *values.iter().min().unwrap(),
        "q1": percentile(&a, 25.0),
        "q3": percentile(&a, 75.0),
        "std": std,
    })
}

// 統計対象から外す状態（レビューで直すまで）
fn excluded(r: &Row) -> bool {
    r.status == "flagged"
}

pub struct Session {
    pub id: String,
    pub dir: PathBuf,
    pub shot_dir: PathBuf,
    pub results_csv: PathBuf,
    pub raw_csv: PathBuf,
    pub start_page: PathBuf,
}

impl Session {
    fn at(id: &str) -> Session {
        let dir = paths::sessions_dir().join(id);
        Session {
            id: id.to_string(),
            shot_dir: dir.join("screenshots"),
            results_csv: dir.join("results.csv"),
            raw_csv: dir.join("rehearsal_data.csv"),
            start_page: dir.join("start_page.png"),
            dir,
        }
    }

    fn write_header(&self) -> std::io::Result<()> {
        let mut text = String::from("\u{feff}");
        text.push_str(&csv_line(&header().iter().map(|s| s.to_string()).collect::<Vec<_>>()));
        fs::write(&self.results_csv, text)
    }

    // ---------- 作成・一覧 ----------
    pub fn create(suffix: &str) -> std::io::Result<Session> {
        let s = Session::at(&(Local::now().format("%Y%m%d_%H%M%S").to_string() + suffix));
        fs::create_dir_all(&s.shot_dir)?;
        s.write_header()?;
        Ok(s)
    }

    pub fn open(id: &str) -> Option<Session> {
        if !valid_id(id) {
            return None;
        }
        let s = Session::at(id);
        if !s.dir.is_dir() {
            return None;
        }
        let _ = fs::create_dir_all(&s.shot_dir);
        if !s.results_csv.exists() {
            let _ = s.write_header();
        }
        Some(s)
    }

    pub fn list_all() -> Vec<Value> {
        let mut names: Vec<String> = fs::read_dir(paths::sessions_dir())
            .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().to_string()).collect())
            .unwrap_or_default();
        names.sort_by(|a, b| b.cmp(a));
        names
            .iter()
            .filter_map(|name| {
                let s = Session::at(name);
                if !s.results_csv.exists() {
                    return None;
                }
                let rows = s.rows();
                Some(json!({
                    "id": name,
                    "iterations": rows.len(),
                    "flagged": rows.iter().filter(|r| r.status == "flagged").count(),
                    "has_start_page": s.start_page.exists(),
                }))
            })
            .collect()
    }

    // ---------- 書き込み ----------
    pub fn screenshot_path(&self, iteration: i64) -> PathBuf {
        self.shot_dir.join(format!("{iteration:04}.png"))
    }

    /// 保存済みスクショの通し番号．
    pub fn screenshot_iterations(&self) -> Vec<i64> {
        let mut its: Vec<i64> = fs::read_dir(&self.shot_dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().to_string())
                    .filter(|n| n.ends_with(".png"))
                    .filter_map(|n| n.get(..4).filter(|p| p.chars().all(|c| c.is_ascii_digit())).and_then(|p| p.parse().ok()))
                    .collect()
            })
            .unwrap_or_default();
        its.sort();
        its
    }

    pub fn next_iteration(&self) -> i64 {
        let done = self.rows().iter().map(|r| r.iteration).max().unwrap_or(0);
        let shots = self.screenshot_iterations().into_iter().max().unwrap_or(0);
        done.max(shots) + 1
    }

    pub fn append_result(&self, iteration: i64, result: &Value) {
        let row = Row::from_result(iteration, Local::now().format("%Y-%m-%dT%H:%M:%S").to_string(), result, "auto");
        let _g = WRITE_LOCK.lock().unwrap();
        if let Ok(mut f) = OpenOptions::new().append(true).create(true).open(&self.results_csv) {
            let _ = f.write_all(csv_line(&row.csv_fields()).as_bytes());
        }
        if row.status != "flagged" {
            if let Ok(mut f) = OpenOptions::new().append(true).create(true).open(&self.raw_csv) {
                // 空き枠は 0（gktools と同じ）
                let _ = f.write_all(raw_line(&row).as_bytes());
            }
        }
    }

    // ---------- 読み込み ----------
    pub fn rows(&self) -> Vec<Row> {
        let Ok(text) = fs::read_to_string(&self.results_csv) else {
            return vec![];
        };
        let mut rdr = csv::ReaderBuilder::new().flexible(true).from_reader(text.trim_start_matches('\u{feff}').as_bytes());
        let head: Vec<String> = match rdr.headers() {
            Ok(h) => h.iter().map(|s| s.to_string()).collect(),
            Err(_) => return vec![],
        };
        let idx = |k: &str| head.iter().position(|h| h == k);
        let mut out = Vec::new();
        for rec in rdr.records().flatten() {
            let get = |k: &str| idx(k).and_then(|i| rec.get(i)).unwrap_or("");
            let Ok(it) = get("iteration").trim().parse::<i64>() else { continue };
            let mut vals = [None; 15];
            for (i, k) in value_cols().enumerate() {
                vals[i] = to_int(get(k));
            }
            let or = |k: &str, d: &str| idx(k).and_then(|i| rec.get(i)).unwrap_or(d).to_string();
            out.push(Row {
                iteration: it,
                timestamp: get("timestamp").to_string(),
                status: or("status", "ok"),
                flags: get("flags").to_string(),
                recovery: or("recovery", "auto"),
                vals,
            });
        }
        out.sort_by_key(|r| r.iteration);
        out
    }

    /// レビュー編集後の全行を書き戻し，rehearsal_data.csv も作り直す．
    pub fn save_rows(&self, rows: &mut [Row]) -> std::io::Result<()> {
        rows.sort_by_key(|r| r.iteration);
        let _g = WRITE_LOCK.lock().unwrap();
        let mut text = String::from("\u{feff}");
        text.push_str(&csv_line(&header().iter().map(|s| s.to_string()).collect::<Vec<_>>()));
        for r in rows.iter() {
            text.push_str(&csv_line(&r.csv_fields()));
        }
        let tmp = self.results_csv.with_extension("csv.tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.results_csv)?;
        let raw: String = rows.iter().filter(|r| !excluded(r)).map(raw_line).collect();
        fs::write(&self.raw_csv, raw)
    }

    // ---------- 統計 ----------
    pub fn statistics(&self) -> Value {
        let all = self.rows();
        let rows: Vec<&Row> = all.iter().filter(|r| !excluded(r)).collect();
        let mut cols = Map::new();
        for k in SCORE_COLS {
            cols.insert(k.into(), describe(&rows.iter().filter_map(|r| r.get(k)).collect::<Vec<_>>()));
        }
        let mut stages = Map::new();
        for (s, k) in TOTAL_COLS.iter().enumerate() {
            stages.insert(format!("S{}", s + 1), describe(&rows.iter().filter_map(|r| r.get(k)).collect::<Vec<_>>()));
        }
        let grand: Vec<i64> = rows
            .iter()
            .filter_map(|r| {
                let t: Option<Vec<i64>> = TOTAL_COLS.iter().map(|k| r.get(k)).collect();
                t.map(|t| t.iter().sum())
            })
            .collect();
        json!({
            "session": self.id,
            "iterations": all.len(),
            "used": rows.len(),
            "flagged": all.iter().filter(|r| r.status == "flagged").count(),
            "columns": cols,
            "stage_totals": stages,
            "grand_total": describe(&grand),
        })
    }

    pub fn write_statistics(&self) -> Value {
        let st = self.statistics();
        let _ = fs::write(self.dir.join("statistics.json"), serde_json::to_string_pretty(&st).unwrap_or_default());
        st
    }

    /// 統計表の CSV（列 = 9 人＋ステージ合計＋3 ステージ計，行 = 統計量）．
    pub fn statistics_csv(&self) -> String {
        let st = self.statistics();
        let keys = [("n", "N"), ("avg", "平均"), ("median", "中央値"), ("max", "最大"), ("min", "最小"), ("q1", "Q1"), ("q3", "Q3"), ("std", "標準偏差")];
        let mut cols: Vec<(String, Value)> = SCORE_COLS.iter().map(|k| (k.to_string(), st["columns"][k].clone())).collect();
        for s in ["S1", "S2", "S3"] {
            cols.push((format!("{s}_total"), st["stage_totals"][s].clone()));
        }
        cols.push(("3stage_total".into(), st["grand_total"].clone()));
        let mut out = String::from("\u{feff}");
        let mut head = vec![String::new()];
        head.extend(cols.iter().map(|(c, _)| c.clone()));
        out.push_str(&csv_line(&head));
        for (k, label) in keys {
            let mut line = vec![label.to_string()];
            line.extend(cols.iter().map(|(_, d)| num_text(&d[k])));
            out.push_str(&csv_line(&line));
        }
        out
    }

    pub fn load_start_page(&self) -> Option<RgbImage> {
        image::open(&self.start_page).ok().map(|i| i.to_rgb8())
    }
}

/// Python の str(float) に近い書き方（整数値の float は "123.0"）．
fn num_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::Number(n) if n.is_f64() => {
            let f = n.as_f64().unwrap();
            if f.fract() == 0.0 {
                format!("{f:.1}")
            } else {
                f.to_string()
            }
        }
        other => other.to_string(),
    }
}

fn raw_line(r: &Row) -> String {
    let vals: Vec<String> = SCORE_COLS.iter().map(|k| r.get(k).unwrap_or(0).to_string()).collect();
    format!("{}\r\n", vals.join(","))
}

/// CSV の 1 行（Python の csv.writer と同じく必要な時だけ引用符で囲む）．
pub fn csv_line(fields: &[String]) -> String {
    let mut w = csv::WriterBuilder::new().terminator(csv::Terminator::CRLF).from_writer(vec![]);
    let _ = w.write_record(fields);
    String::from_utf8(w.into_inner().unwrap_or_default()).unwrap_or_default()
}

// --------------------------------------------------------------------------
// セッションの結合
// --------------------------------------------------------------------------
pub fn column_counts(s: &Session) -> Vec<usize> {
    let rows: Vec<Row> = s.rows().into_iter().filter(|r| !excluded(r)).collect();
    SCORE_COLS.iter().map(|k| rows.iter().filter(|r| r.get(k).unwrap_or(0) != 0).count()).collect()
}

/// 編成画面のアイコンを比べて，結合元のどの枠を結合先のどの枠に足すかを提案する．
/// mapping[j] = 結合先の枠 j に足す結合元の枠番号（None = 足さない）．
pub fn suggest_mapping(base: &Session, source: &Session, icons: &[[f64; 4]]) -> Value {
    let max_distance = 20.0;
    let counts = column_counts(source);
    let mut dist: Vec<Vec<Option<f64>>> = vec![vec![None; 9]; 9];
    let mut mapping: Vec<Option<usize>> = vec![None; 9];
    match (base.load_start_page(), source.load_start_page()) {
        (Some(a), Some(b)) if icons.len() >= 9 => {
            let ia: Vec<Vec<f32>> = icons.iter().map(|&r| icon_small(&a, r)).collect();
            let ib: Vec<Vec<f32>> = icons.iter().map(|&r| icon_small(&b, r)).collect();
            let mut pairs = Vec::new();
            for j in 0..9 {
                for i in 0..9 {
                    let d = round1(icon_distance(&ia[j], &ib[i]));
                    dist[j][i] = Some(d);
                    if counts[i] > 0 {
                        pairs.push((d, j, i));
                    }
                }
            }
            // 似ている組から順に 1 対 1 で割り当て（データの無い枠は使わない）
            pairs.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
            let mut used = [false; 9];
            for (d, j, i) in pairs {
                if d > max_distance {
                    break;
                }
                if mapping[j].is_none() && !used[i] {
                    mapping[j] = Some(i);
                    used[i] = true;
                }
            }
        }
        // 編成画像が無ければ同じ位置どうし
        _ => mapping = (0..9).map(|i| (counts[i] > 0).then_some(i)).collect(),
    }
    json!({"mapping": mapping, "distances": dist, "source_counts": counts, "base_counts": column_counts(base)})
}

/// base の全行のあとに source の行を mapping どおり並べ替えて足した，新しいセッションを作る．
/// 元の 2 つのセッションは変更しない．
pub fn merge_sessions(base: &Session, source: &Session, mapping: &[Option<usize>]) -> std::io::Result<Session> {
    let new = Session::create("_merged")?;
    if base.start_page.exists() {
        fs::copy(&base.start_page, &new.start_page)?;
    }
    if source.start_page.exists() {
        fs::copy(&source.start_page, new.dir.join(format!("source_start_page_{}.png", source.id)))?;
    }
    let base_rows = base.rows();
    let source_rows = source.rows();
    let mut rows = base_rows.clone();
    let mut next_it = rows.iter().map(|r| r.iteration).max().unwrap_or(0) + 1;
    for r in &source_rows {
        let tag = format!("merged:{}#{}", source.id, r.iteration);
        let mut out = Row {
            iteration: next_it,
            timestamp: r.timestamp.clone(),
            status: r.status.clone(),
            flags: if r.flags.is_empty() { tag } else { format!("{};{}", r.flags, tag) },
            recovery: r.recovery.clone(),
            vals: [None; 15],
        };
        for (j, k) in SCORE_COLS.iter().enumerate() {
            out.set(k, mapping[j].and_then(|m| r.get(SCORE_COLS[m])));
        }
        for s in 0..3 {
            // ステージをまたいで並べ替えた場合，元の合計・ボーナスは意味が無いので空欄
            let same = (0..3).all(|c| mapping[s * 3 + c] == Some(s * 3 + c));
            out.set(TOTAL_COLS[s], if same { r.get(TOTAL_COLS[s]) } else { None });
            out.set(BONUS_COLS[s], if same { r.get(BONUS_COLS[s]) } else { None });
        }
        rows.push(out);
        next_it += 1;
    }
    new.save_rows(&mut rows)?;
    let map_info: Map<String, Value> = SCORE_COLS
        .iter()
        .enumerate()
        .map(|(j, k)| (k.to_string(), json!(mapping[j].map(|m| SCORE_COLS[m]))))
        .collect();
    let info = json!({"base": base.id, "source": source.id, "mapping": map_info,
                      "base_rows": base_rows.len(), "source_rows": source_rows.len()});
    let _ = fs::write(new.dir.join("merge_info.json"), serde_json::to_string_pretty(&info).unwrap_or_default());
    new.write_statistics();
    Ok(new)
}
