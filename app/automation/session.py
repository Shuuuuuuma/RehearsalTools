"""周回セッションの保存：スクショ・CSV・統計・グラフ．

sessions/<YYYYmmdd_HHMMSS>/
  start_page.png        1 周目の開始前に撮ったリハーサル画面（編成チェック・グラフのラベル用）
  screenshots/0001.png  各周の結果画面
  results.csv           周ごとの OCR 結果（ヘッダ付き，状態・フラグ付き）
  rehearsal_data.csv    9 人分のスコアのみ（ヘッダなし，表計算に貼り付け用）
  statistics.json       列ごとの統計
  charts/               グラフ PNG
"""
import csv
import json
import os
import threading
from datetime import datetime
from typing import Any, Dict, List, Optional

import numpy as np
from PIL import Image

from app.automation.config import SESSIONS_DIR

SCORE_COLS = [f"S{s}C{c}" for s in (1, 2, 3) for c in (1, 2, 3)]
TOTAL_COLS = [f"S{s}_total" for s in (1, 2, 3)]
BONUS_COLS = [f"S{s}_bonus" for s in (1, 2, 3)]
HEADER = ["iteration", "timestamp"] + SCORE_COLS + TOTAL_COLS + BONUS_COLS + ["status", "flags", "recovery"]
# 統計対象から外す状態（レビューで直すまで）
EXCLUDED = {"flagged"}


def _to_int(v) -> Optional[int]:
    try:
        return int(str(v).replace(",", "")) if str(v).strip() not in ("", "None") else None
    except ValueError:
        return None


def describe(values: List[int]) -> Dict[str, Any]:
    if not values:
        return {"n": 0, "avg": None, "median": None, "max": None, "min": None, "q1": None, "q3": None, "std": None}
    a = np.asarray(values, dtype=np.float64)
    return {
        "n": int(a.size),
        "avg": round(float(a.mean()), 1),
        "median": float(np.median(a)),
        "max": int(a.max()),
        "min": int(a.min()),
        "q1": float(np.percentile(a, 25)),
        "q3": float(np.percentile(a, 75)),
        "std": round(float(a.std(ddof=1)), 1) if a.size > 1 else 0.0,
    }


class Session:
    def __init__(self, session_id: str):
        self.id = session_id
        self.dir = os.path.join(SESSIONS_DIR, session_id)
        self.shot_dir = os.path.join(self.dir, "screenshots")
        self.chart_dir = os.path.join(self.dir, "charts")
        self.results_csv = os.path.join(self.dir, "results.csv")
        self.raw_csv = os.path.join(self.dir, "rehearsal_data.csv")
        self.start_page = os.path.join(self.dir, "start_page.png")
        self._lock = threading.Lock()

    # ---------- 作成・一覧 ----------
    @classmethod
    def create(cls) -> "Session":
        sid = datetime.now().strftime("%Y%m%d_%H%M%S")
        s = cls(sid)
        os.makedirs(s.shot_dir, exist_ok=True)
        os.makedirs(s.chart_dir, exist_ok=True)
        with open(s.results_csv, "w", newline="", encoding="utf-8-sig") as f:
            csv.writer(f).writerow(HEADER)
        return s

    @classmethod
    def open(cls, session_id: str) -> "Session":
        s = cls(session_id)
        if not os.path.isdir(s.dir):
            raise FileNotFoundError(session_id)
        os.makedirs(s.shot_dir, exist_ok=True)
        os.makedirs(s.chart_dir, exist_ok=True)
        if not os.path.exists(s.results_csv):
            with open(s.results_csv, "w", newline="", encoding="utf-8-sig") as f:
                csv.writer(f).writerow(HEADER)
        return s

    @staticmethod
    def list_all() -> List[Dict[str, Any]]:
        out = []
        if not os.path.isdir(SESSIONS_DIR):
            return out
        for name in sorted(os.listdir(SESSIONS_DIR), reverse=True):
            s = Session(name)
            if not os.path.exists(s.results_csv):
                continue
            rows = s.rows()
            out.append({
                "id": name,
                "iterations": len(rows),
                "flagged": sum(1 for r in rows if r["status"] == "flagged"),
                "has_start_page": os.path.exists(s.start_page),
            })
        return out

    # ---------- 書き込み ----------
    def screenshot_path(self, iteration: int) -> str:
        return os.path.join(self.shot_dir, f"{iteration:04d}.png")

    def next_iteration(self) -> int:
        rows = self.rows()
        done = max([r["iteration"] for r in rows], default=0)
        shots = [int(f[:4]) for f in os.listdir(self.shot_dir) if f[:4].isdigit()] if os.path.isdir(self.shot_dir) else []
        return max(done, max(shots, default=0)) + 1

    def append_result(self, iteration: int, result: Dict[str, Any]):
        stages = result["stages"]
        scores = [stages[s]["scores"][c] for s in range(3) for c in range(3)]
        totals = [stages[s]["total"] for s in range(3)]
        bonuses = [stages[s]["bonus"] for s in range(3)]
        row = [iteration, datetime.now().strftime("%Y-%m-%dT%H:%M:%S")] + scores + totals + bonuses + \
              [result["status"], result.get("flags", ""), result.get("recovery", "auto")]
        with self._lock:
            with open(self.results_csv, "a", newline="", encoding="utf-8-sig") as f:
                csv.writer(f).writerow(["" if v is None else v for v in row])
            if result["status"] != "flagged":
                with open(self.raw_csv, "a", newline="", encoding="utf-8") as f:
                    csv.writer(f).writerow([v or 0 for v in scores])  # 空き枠は 0（gktools と同じ）

    # ---------- 読み込み ----------
    def rows(self) -> List[Dict[str, Any]]:
        if not os.path.exists(self.results_csv):
            return []
        out = []
        with open(self.results_csv, newline="", encoding="utf-8-sig") as f:
            for r in csv.DictReader(f):
                try:
                    it = int(r["iteration"])
                except (KeyError, ValueError):
                    continue
                row = {"iteration": it, "timestamp": r.get("timestamp", ""),
                       "status": r.get("status", "ok"), "flags": r.get("flags", ""),
                       "recovery": r.get("recovery", "auto")}
                for k in SCORE_COLS + TOTAL_COLS + BONUS_COLS:
                    row[k] = _to_int(r.get(k))
                out.append(row)
        out.sort(key=lambda x: x["iteration"])
        return out

    def save_rows(self, rows: List[Dict[str, Any]]):
        """レビュー編集後の全行を書き戻し，rehearsal_data.csv も作り直す．"""
        rows = sorted(rows, key=lambda x: x["iteration"])
        with self._lock:
            tmp = self.results_csv + ".tmp"
            with open(tmp, "w", newline="", encoding="utf-8-sig") as f:
                w = csv.writer(f)
                w.writerow(HEADER)
                for r in rows:
                    w.writerow(["" if r.get(k) is None else r.get(k) for k in HEADER])
            os.replace(tmp, self.results_csv)
            with open(self.raw_csv, "w", newline="", encoding="utf-8") as f:
                w = csv.writer(f)
                for r in rows:
                    if r["status"] not in EXCLUDED:
                        w.writerow([r[k] or 0 for k in SCORE_COLS])

    # ---------- 統計 ----------
    def statistics(self) -> Dict[str, Any]:
        rows = [r for r in self.rows() if r["status"] not in EXCLUDED]
        cols = {}
        for k in SCORE_COLS:
            cols[k] = describe([r[k] for r in rows if r[k] is not None])
        stages = {}
        for s in (1, 2, 3):
            vals = [r[f"S{s}_total"] for r in rows if r[f"S{s}_total"] is not None]
            stages[f"S{s}"] = describe(vals)
        grand = [sum(r[f"S{s}_total"] for s in (1, 2, 3)) for r in rows
                 if all(r[f"S{s}_total"] is not None for s in (1, 2, 3))]
        all_rows = self.rows()
        return {
            "session": self.id,
            "iterations": len(all_rows),
            "used": len(rows),
            "flagged": sum(1 for r in all_rows if r["status"] == "flagged"),
            "columns": cols,
            "stage_totals": stages,
            "grand_total": describe(grand),
        }

    def write_statistics(self) -> Dict[str, Any]:
        st = self.statistics()
        with open(os.path.join(self.dir, "statistics.json"), "w", encoding="utf-8") as f:
            json.dump(st, f, ensure_ascii=False, indent=2)
        return st

    # ---------- グラフ ----------
    def generate_charts(self, icon_regions: Optional[List[List[float]]] = None) -> List[str]:
        import matplotlib
        matplotlib.use("Agg")
        import matplotlib.pyplot as plt
        from matplotlib.offsetbox import AnnotationBbox, OffsetImage

        for fam in ("Yu Gothic", "Meiryo", "MS Gothic", "Noto Sans CJK JP", "IPAexGothic"):
            try:
                from matplotlib import font_manager
                font_manager.findfont(fam, fallback_to_default=False)
                plt.rcParams["font.family"] = fam
                break
            except Exception:
                continue

        rows = [r for r in self.rows() if r["status"] not in EXCLUDED]
        os.makedirs(self.chart_dir, exist_ok=True)
        written = []
        if not rows:
            return written

        data = [[r[k] for r in rows if r[k] is not None] for k in SCORE_COLS]
        icons = []
        if icon_regions and os.path.exists(self.start_page):
            from app.automation.detector import crop_rel
            sp = Image.open(self.start_page).convert("RGB")
            icons = [crop_rel(sp, reg) for reg in icon_regions]

        # 9 人の箱ひげ図
        fig, ax = plt.subplots(figsize=(13, 6.5), dpi=110)
        ax.boxplot(data, showmeans=True, widths=0.55)
        ax.set_xticks(range(1, 10))
        ax.set_xticklabels(SCORE_COLS, fontsize=8)
        ax.set_title(f"スコア分布（{len(rows)} 回）  session {self.id}")
        ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{int(v):,}"))
        ax.grid(axis="y", alpha=0.3)
        for i in (3.5, 6.5):
            ax.axvline(i, color="#999", lw=0.8, ls="--")
        if icons:
            ax.tick_params(axis="x", pad=46)
            for i, ic in enumerate(icons):
                ic = ic.resize((44, int(44 * ic.height / max(1, ic.width))))
                ab = AnnotationBbox(OffsetImage(np.asarray(ic), zoom=1), (i + 1, 0),
                                    xycoords=("data", "axes fraction"), box_alignment=(0.5, 1.1), frameon=False)
                ax.add_artist(ab)
        fig.tight_layout()
        if icons:
            fig.subplots_adjust(bottom=0.2)
        p = os.path.join(self.chart_dir, "boxplot.png")
        fig.savefig(p)
        plt.close(fig)
        written.append(p)

        # 列ごとのヒストグラム
        for k, vals in zip(SCORE_COLS, data):
            if not vals:
                continue
            fig, ax = plt.subplots(figsize=(6.5, 4), dpi=100)
            ax.hist(vals, bins=min(30, max(5, len(vals) // 3)), color="#e05a8a", edgecolor="white")
            st = describe(vals)
            ax.axvline(st["avg"], color="#333", ls="--", lw=1, label=f"Ave {st['avg']:,.0f}")
            ax.axvline(st["median"], color="#2a7", ls=":", lw=1, label=f"Med {st['median']:,.0f}")
            ax.set_title(f"{k}（n={st['n']}）")
            ax.xaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{int(v):,}"))
            ax.legend(fontsize=8)
            fig.tight_layout()
            p = os.path.join(self.chart_dir, f"hist_{k}.png")
            fig.savefig(p)
            plt.close(fig)
            written.append(p)

        # ステージ合計の推移
        fig, ax = plt.subplots(figsize=(10, 4), dpi=100)
        its = [r["iteration"] for r in rows]
        for s in (1, 2, 3):
            ax.plot(its, [r[f"S{s}_total"] for r in rows], lw=1, label=f"ステージ{s}")
        ax.set_xlabel("周回")
        ax.yaxis.set_major_formatter(matplotlib.ticker.FuncFormatter(lambda v, _: f"{int(v):,}"))
        ax.legend()
        ax.grid(alpha=0.3)
        fig.tight_layout()
        p = os.path.join(self.chart_dir, "stage_totals.png")
        fig.savefig(p)
        plt.close(fig)
        written.append(p)
        return written


# --------------------------------------------------------------------------
# セッションの結合
# --------------------------------------------------------------------------
def column_counts(session: "Session") -> List[int]:
    rows = [r for r in session.rows() if r["status"] not in EXCLUDED]
    return [sum(1 for r in rows if r[k]) for k in SCORE_COLS]


def suggest_mapping(base: "Session", source: "Session", icon_regions: List[List[float]],
                    max_distance: float = 20.0) -> Dict[str, Any]:
    """編成画面のアイコンを比べて，結合元のどの枠を結合先のどの枠に足すかを提案する．
    mapping[j] = 結合先の枠 j に足す結合元の枠番号（None = 足さない）．"""
    counts = column_counts(source)
    dist = [[None] * 9 for _ in range(9)]
    if os.path.exists(base.start_page) and os.path.exists(source.start_page):
        from app.automation.detector import ScreenDetector, crop_rel
        a = Image.open(base.start_page).convert("RGB")
        b = Image.open(source.start_page).convert("RGB")
        ia = [np.asarray(crop_rel(a, r).convert("L").resize((16, 16)), dtype=np.float32) for r in icon_regions]
        ib = [np.asarray(crop_rel(b, r).convert("L").resize((16, 16)), dtype=np.float32) for r in icon_regions]
        for j in range(9):
            for i in range(9):
                dist[j][i] = round(float(np.abs(ia[j] - ib[i]).mean()), 1)
        # 似ている組から順に 1 対 1 で割り当て（データの無い枠は使わない）
        pairs = sorted((dist[j][i], j, i) for j in range(9) for i in range(9) if counts[i] > 0)
        mapping: List[Optional[int]] = [None] * 9
        used = set()
        for d, j, i in pairs:
            if d > max_distance:
                break
            if mapping[j] is None and i not in used:
                mapping[j] = i
                used.add(i)
    else:
        # 編成画像が無ければ同じ位置どうし
        mapping = [i if counts[i] > 0 else None for i in range(9)]
    return {"mapping": mapping, "distances": dist, "source_counts": counts, "base_counts": column_counts(base)}


def merge_sessions(base: "Session", source: "Session", mapping: List[Optional[int]],
                   icon_regions: Optional[List[List[float]]] = None) -> "Session":
    """base の全行のあとに source の行を mapping どおり並べ替えて足した，新しいセッションを作る．
    元の 2 つのセッションは変更しない．"""
    import shutil
    if len(mapping) != 9:
        raise ValueError("mapping は 9 個必要です")
    new = Session(datetime.now().strftime("%Y%m%d_%H%M%S") + "_merged")
    os.makedirs(new.shot_dir, exist_ok=True)
    os.makedirs(new.chart_dir, exist_ok=True)
    if os.path.exists(base.start_page):
        shutil.copy(base.start_page, new.start_page)
    if os.path.exists(source.start_page):
        shutil.copy(source.start_page, os.path.join(new.dir, f"source_start_page_{source.id}.png"))

    rows = [dict(r) for r in base.rows()]
    next_it = max([r["iteration"] for r in rows], default=0) + 1
    for r in source.rows():
        out = {"iteration": next_it, "timestamp": r["timestamp"], "status": r["status"],
               "flags": ";".join(x for x in (r.get("flags"), f"merged:{source.id}#{r['iteration']}") if x),
               "recovery": r.get("recovery", "auto")}
        for j, k in enumerate(SCORE_COLS):
            m = mapping[j]
            out[k] = r[SCORE_COLS[m]] if m is not None else None
        for s in range(3):
            same = all(mapping[s * 3 + c] == s * 3 + c for c in range(3))
            # ステージをまたいで並べ替えた場合，元の合計・ボーナスは意味が無いので空欄
            out[TOTAL_COLS[s]] = r[TOTAL_COLS[s]] if same else None
            out[BONUS_COLS[s]] = r[BONUS_COLS[s]] if same else None
        rows.append(out)
        next_it += 1

    new.save_rows(rows)
    with open(os.path.join(new.dir, "merge_info.json"), "w", encoding="utf-8") as f:
        json.dump({"base": base.id, "source": source.id,
                   "mapping": {SCORE_COLS[j]: (SCORE_COLS[m] if m is not None else None) for j, m in enumerate(mapping)},
                   "base_rows": len(base.rows()), "source_rows": len(source.rows())},
                  f, ensure_ascii=False, indent=2)
    new.write_statistics()
    try:
        new.generate_charts(icon_regions)
    except Exception as e:
        print(f"[merge] グラフ出力失敗: {e}")
    return new
