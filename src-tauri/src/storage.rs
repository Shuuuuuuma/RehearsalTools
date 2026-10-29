//! ダッシュボード用のスコア DB（data/rehearsal_scores.db．Python 版と同じ SQLite ファイル）．

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::Local;
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};

use crate::paths;

pub struct ScoreStorage {
    conn: Mutex<Connection>,
}

/// 1 ステージ分のスコア（stage は 1〜3，scores は枠 1〜3．None・0 は登録しない）．
pub struct StageScores {
    pub stage: i64,
    pub scores: Vec<Option<i64>>,
}

impl ScoreStorage {
    pub fn open() -> Result<Self, String> {
        let dir = paths::data_dir();
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let conn = Connection::open(dir.join("rehearsal_scores.db")).map_err(|e| e.to_string())?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS rehearsal_scores (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                trial_id TEXT NOT NULL,
                stage INTEGER NOT NULL,
                slot INTEGER NOT NULL,
                idol_name TEXT NOT NULL,
                score INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS idol_custom_names (
                stage INTEGER NOT NULL,
                slot INTEGER NOT NULL,
                custom_name TEXT NOT NULL,
                PRIMARY KEY (stage, slot)
            );",
        )
        .map_err(|e| e.to_string())?;
        Ok(ScoreStorage { conn: Mutex::new(conn) })
    }

    /// 各枠のキャラ名（カスタム名）を設定・保存します．
    pub fn set_idol_name(&self, stage: i64, slot: i64, name: &str) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO idol_custom_names (stage, slot, custom_name) VALUES (?1, ?2, ?3)
             ON CONFLICT(stage, slot) DO UPDATE SET custom_name = excluded.custom_name",
            params![stage, slot, name],
        )?;
        Ok(())
    }

    /// 設定された全枠のキャラ名マッピング（stg1_slot1 → 名前）．
    pub fn idol_names(&self) -> Map<String, Value> {
        let mut names = Map::new();
        for st in 1..=3 {
            for slot in 1..=3 {
                names.insert(format!("stg{st}_slot{slot}"), json!(format!("ステージ{st}-枠{slot}")));
            }
        }
        let conn = self.conn.lock().unwrap();
        if let Ok(mut q) = conn.prepare("SELECT stage, slot, custom_name FROM idol_custom_names") {
            let rows = q.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(2)?)));
            for (st, slot, name) in rows.into_iter().flatten().flatten() {
                names.insert(format!("stg{st}_slot{slot}"), json!(name));
            }
        }
        names
    }

    /// 1 回のリハーサルのスコアを一括登録します．
    pub fn add_trial_scores(&self, trial_id: &str, stages: &[StageScores]) -> rusqlite::Result<()> {
        let now = Local::now().format("%Y-%m-%dT%H:%M:%S%.6f").to_string();
        let names = self.idol_names();
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        for st in stages {
            for (i, score) in st.scores.iter().enumerate() {
                let Some(score) = score.filter(|&v| v > 0) else { continue };
                let slot = i as i64 + 1;
                let key = format!("stg{}_slot{slot}", st.stage);
                let name = names.get(&key).and_then(|v| v.as_str()).map(|s| s.to_string()).unwrap_or_else(|| format!("ステージ{}-枠{slot}", st.stage));
                tx.execute(
                    "INSERT INTO rehearsal_scores (created_at, trial_id, stage, slot, idol_name, score) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![now, trial_id, st.stage, slot, name, score],
                )?;
            }
        }
        tx.commit()
    }

    /// 直近の試行データ．
    pub fn recent_trials(&self, limit: i64) -> Vec<Value> {
        let conn = self.conn.lock().unwrap();
        let Ok(mut q) = conn.prepare(
            "SELECT trial_id, created_at, stage, slot, idol_name, score FROM rehearsal_scores ORDER BY id DESC LIMIT ?1",
        ) else {
            return vec![];
        };
        q.query_map([limit], |r| {
            Ok(json!({
                "trial_id": r.get::<_, String>(0)?,
                "created_at": r.get::<_, Option<String>>(1)?,
                "stage": r.get::<_, i64>(2)?,
                "slot": r.get::<_, i64>(3)?,
                "idol_name": r.get::<_, String>(4)?,
                "score": r.get::<_, i64>(5)?,
            }))
        })
        .map(|it| it.flatten().collect())
        .unwrap_or_default()
    }

    /// リハーサル分析カード用：試行ごとの 3 ステージ × 3 人のスコア（登録順）．
    pub fn trial_grid(&self) -> Vec<(String, [[i64; 3]; 3])> {
        let conn = self.conn.lock().unwrap();
        let mut order: Vec<String> = Vec::new();
        let mut trials: HashMap<String, [[i64; 3]; 3]> = HashMap::new();
        if let Ok(mut q) = conn.prepare("SELECT trial_id, stage, slot, score FROM rehearsal_scores ORDER BY id") {
            let rows = q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)));
            for (tid, stg, slot, score) in rows.into_iter().flatten().flatten() {
                let grid = trials.entry(tid.clone()).or_insert_with(|| {
                    order.push(tid.clone());
                    [[0; 3]; 3]
                });
                if (1..=3).contains(&stg) && (1..=3).contains(&slot) {
                    grid[stg as usize - 1][slot as usize - 1] = score;
                }
            }
        }
        order.into_iter().map(|t| { let g = trials[&t]; (t, g) }).collect()
    }

    /// 全スコアの CSV（Excel で開けるよう UTF-8 BOM 付き）．
    pub fn export_csv(&self) -> String {
        let conn = self.conn.lock().unwrap();
        let mut out = String::from("\u{feff}");
        out.push_str(&crate::session::csv_line(&["trial_id", "created_at", "stage", "slot", "idol_name", "score"].map(String::from)));
        if let Ok(mut q) = conn.prepare("SELECT trial_id, created_at, stage, slot, idol_name, score FROM rehearsal_scores ORDER BY id") {
            let rows = q.query_map([], |r| {
                Ok(vec![
                    r.get::<_, String>(0)?,
                    r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    r.get::<_, i64>(2)?.to_string(),
                    r.get::<_, i64>(3)?.to_string(),
                    r.get::<_, String>(4)?,
                    r.get::<_, i64>(5)?.to_string(),
                ])
            });
            for rec in rows.into_iter().flatten().flatten() {
                out.push_str(&crate::session::csv_line(&rec));
            }
        }
        out
    }

    /// 全試行データを消去します．
    pub fn clear_all(&self) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute("DELETE FROM rehearsal_scores", [])?;
        Ok(())
    }
}
