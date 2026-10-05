import sqlite3
import os
from typing import List, Dict, Any, Optional
from datetime import datetime

from app.paths import DATA_ROOT

DB_PATH = os.path.join(DATA_ROOT, "data", "rehearsal_scores.db")

class ScoreStorage:
    """リハーサル試行データの保存と取得を行うストレージクラス．"""

    def __init__(self, db_path: str = DB_PATH):
        self.db_path = db_path
        os.makedirs(os.path.dirname(self.db_path), exist_ok=True)
        self.init_db()

    def get_connection(self):
        return sqlite3.connect(self.db_path)

    def init_db(self):
        with self.get_connection() as conn:
            cursor = conn.cursor()
            cursor.execute("""
                CREATE TABLE IF NOT EXISTS rehearsal_scores (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
                    trial_id TEXT NOT NULL,
                    stage INTEGER NOT NULL,
                    slot INTEGER NOT NULL,
                    idol_name TEXT NOT NULL,
                    score INTEGER NOT NULL
                )
            """)
            cursor.execute("""
                CREATE TABLE IF NOT EXISTS idol_custom_names (
                    stage INTEGER NOT NULL,
                    slot INTEGER NOT NULL,
                    custom_name TEXT NOT NULL,
                    PRIMARY KEY (stage, slot)
                )
            """)
            conn.commit()

    def set_idol_name(self, stage: int, slot: int, name: str):
        """各枠のキャラ名（カスタム名）を設定・保存します．"""
        with self.get_connection() as conn:
            cursor = conn.cursor()
            cursor.execute("""
                INSERT INTO idol_custom_names (stage, slot, custom_name)
                VALUES (?, ?, ?)
                ON CONFLICT(stage, slot) DO UPDATE SET custom_name = excluded.custom_name
            """, (stage, slot, name))
            conn.commit()

    def get_all_idol_names(self) -> Dict[str, str]:
        """設定された全枠のキャラ名マッピングを取得します．"""
        names = {}
        # デフォルト名
        for stg in [1, 2, 3]:
            for slot in [1, 2, 3]:
                names[f"stg{stg}_slot{slot}"] = f"ステージ{stg}-枠{slot}"

        with self.get_connection() as conn:
            cursor = conn.cursor()
            cursor.execute("SELECT stage, slot, custom_name FROM idol_custom_names")
            for r in cursor.fetchall():
                names[f"stg{r[0]}_slot{r[1]}"] = r[2]
        return names

    def add_trial_scores(self, trial_id: str, stages_data: List[Dict[str, Any]]):
        """1回のリハーサルのスコアを一括登録します．"""
        now = datetime.now().isoformat()
        idol_names = self.get_all_idol_names()
        records = []

        for stage_info in stages_data:
            stg = stage_info.get("stage")
            scores = stage_info.get("scores", [])
            for slot_idx, score in enumerate(scores):
                if score is not None and score > 0:
                    slot = slot_idx + 1
                    name = idol_names.get(f"stg{stg}_slot{slot}", f"ステージ{stg}-枠{slot}")
                    records.append((now, trial_id, stg, slot, name, int(score)))

        with self.get_connection() as conn:
            cursor = conn.cursor()
            cursor.executemany("""
                INSERT INTO rehearsal_scores (created_at, trial_id, stage, slot, idol_name, score)
                VALUES (?, ?, ?, ?, ?, ?)
            """, records)
            conn.commit()

    def get_scores_by_stage_and_slot(self, stage: int, slot: int) -> List[int]:
        """特定ステージの特定枠（キャラ）のスコア一覧を取得します．"""
        with self.get_connection() as conn:
            cursor = conn.cursor()
            cursor.execute(
                "SELECT score FROM rehearsal_scores WHERE stage = ? AND slot = ? ORDER BY id DESC",
                (stage, slot)
            )
            return [r[0] for r in cursor.fetchall()]

    def get_all_scores(self, target_filter: Optional[str] = None) -> List[int]:
        """指定されたフィルター（ステージや個別枠）の全スコアを取得します．"""
        with self.get_connection() as conn:
            cursor = conn.cursor()
            if target_filter:
                if target_filter.startswith("stg") and "_slot" in target_filter:
                    # 例: stg1_slot2
                    parts = target_filter.replace("stg", "").split("_slot")
                    stg, slot = int(parts[0]), int(parts[1])
                    cursor.execute("SELECT score FROM rehearsal_scores WHERE stage = ? AND slot = ? ORDER BY id DESC", (stg, slot))
                elif target_filter.startswith("stage_"):
                    stg = int(target_filter.split("_")[1])
                    cursor.execute("SELECT score FROM rehearsal_scores WHERE stage = ? ORDER BY id DESC", (stg,))
                else:
                    cursor.execute("SELECT score FROM rehearsal_scores WHERE idol_name = ? ORDER BY id DESC", (target_filter,))
            else:
                cursor.execute("SELECT score FROM rehearsal_scores ORDER BY id DESC")
            return [r[0] for r in cursor.fetchall()]

    def get_recent_trials(self, limit: int = 50) -> List[Dict[str, Any]]:
        """直近の試行データを取得します．"""
        with self.get_connection() as conn:
            conn.row_factory = sqlite3.Row
            cursor = conn.cursor()
            cursor.execute("""
                SELECT trial_id, created_at, stage, slot, idol_name, score
                FROM rehearsal_scores
                ORDER BY id DESC
                LIMIT ?
            """, (limit,))
            return [dict(r) for r in cursor.fetchall()]

    def clear_all(self):
        """全試行データを消去します．"""
        with self.get_connection() as conn:
            cursor = conn.cursor()
            cursor.execute("DELETE FROM rehearsal_scores")
            conn.commit()
