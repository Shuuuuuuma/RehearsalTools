import os
import uuid
import base64
import io
from typing import Optional, Dict
from fastapi import FastAPI, HTTPException, Query
from fastapi.responses import HTMLResponse, FileResponse, Response
from fastapi.staticfiles import StaticFiles
from pydantic import BaseModel
from PIL import Image

from app.capture import WindowCapture
from app.ocr_parser import RehearsalOcrParser
from app.storage import ScoreStorage
from app.analytics import RehearsalAnalytics
from app.scanner import AutoResultScanner

app = FastAPI(title="学マス リハーサル結果自動集計＆分析ツール")

capture_mgr = WindowCapture(target_title="gakumas")
ocr_parser = RehearsalOcrParser()
storage = ScoreStorage()
scanner = AutoResultScanner(capture_mgr, ocr_parser, storage)

# ---- 自動周回 ----
from app.automation.api import build_router
from app.automation.config import ConfigStore
from app.automation.hotkeys import HotkeyThread
from app.automation.runner import AutomationRunner


def _store_auto_result(session_id: str, iteration: int, res: dict):
    """自動周回の OCR 結果を既存ダッシュボード用 DB にも登録（要確認の結果は除く）．"""
    if res.get("status") == "flagged":
        return
    stages = [{"stage": i + 1, "scores": s["scores"]} for i, s in enumerate(res["stages"])]
    storage.add_trial_scores(f"{session_id}-{iteration:04d}", stages)


auto_config = ConfigStore()
auto_runner = AutomationRunner(auto_config, on_result=_store_auto_result)


def _hotkey_start():
    if not auto_runner.running:
        auto_runner.start(auto_runner.last_iterations)


def _hotkey_screenshot():
    auto_runner.screenshot()


hotkey_thread = HotkeyThread({"A": _hotkey_start, "Q": auto_runner.abort, "S": _hotkey_screenshot})
hotkey_thread.start()
app.include_router(build_router(auto_config, auto_runner, {"registered": hotkey_thread.registered}))

from app.paths import DATA_ROOT, RESOURCE_DIR
STATIC_DIR = os.path.join(RESOURCE_DIR, "static")
DATA_DIR = os.path.join(DATA_ROOT, "data")
os.makedirs(STATIC_DIR, exist_ok=True)
os.makedirs(DATA_DIR, exist_ok=True)
app.mount("/static", StaticFiles(directory=STATIC_DIR), name="static")

LAST_CAPTURE_PATH = os.path.join(DATA_DIR, "last_capture.png")

class CaptureRequest(BaseModel):
    hwnd: Optional[int] = None
    target_title: Optional[str] = "gakumas"

class SetIdolNameRequest(BaseModel):
    stage: int
    slot: int
    name: str

class Base64ImageRequest(BaseModel):
    image_base64: str

@app.get("/api/windows")
def get_windows():
    """現在PC上で開いている可視ウィンドウ一覧を返します．"""
    windows = capture_mgr.list_visible_windows()
    target_hwnd = capture_mgr.find_target_window()
    return {
        "target_hwnd": target_hwnd,
        "windows": windows
    }

@app.post("/api/capture")
async def capture_and_parse(req: CaptureRequest):
    """指定ウィンドウ（またはgakumas）をキャプチャし，OCR解析して記録します．"""
    hwnd = req.hwnd or capture_mgr.find_target_window()
    if not hwnd:
        raise HTTPException(
            status_code=404,
            detail="ゲームウィンドウ（gakumas）が見つかりません．ゲームが起動しているか確認してください．"
        )

    image = capture_mgr.capture(hwnd)
    if not image:
        raise HTTPException(
            status_code=500,
            detail="ウィンドウのキャプチャに失敗しました．ウィンドウが最小化されていないか確認してください．"
        )

    image.save(LAST_CAPTURE_PATH)

    parse_result = await ocr_parser.parse_image(image)
    total_scores = sum(len(stg["scores"]) for stg in parse_result["stages"])
    if total_scores == 0:
        raise HTTPException(
            status_code=400,
            detail="スコアが検出できませんでした．学マスのゲーム画面（リハーサル結果）が手前に表示されているか確認してください．"
        )

    trial_id = str(uuid.uuid4())[:8]
    storage.add_trial_scores(trial_id, parse_result["stages"])

    return {
        "success": True,
        "trial_id": trial_id,
        "detected_count": total_scores,
        "stages": parse_result["stages"]
    }

@app.post("/api/upload_image")
async def upload_image(req: Base64ImageRequest):
    """クリップボード（Ctrl+V）等から直接貼り付けられた画像を解析して記録します．"""
    try:
        data_str = req.image_base64
        if "," in data_str:
            data_str = data_str.split(",", 1)[1]
        img_bytes = base64.b64decode(data_str)
        image = Image.open(io.BytesIO(img_bytes)).convert("RGB")
    except Exception as e:
        raise HTTPException(status_code=400, detail=f"画像のデコードに失敗しました: {e}")

    image.save(LAST_CAPTURE_PATH)

    parse_result = await ocr_parser.parse_image(image)
    total_scores = sum(len(stg["scores"]) for stg in parse_result["stages"])
    if total_scores == 0:
        raise HTTPException(
            status_code=400,
            detail="画像からスコアが検出できませんでした．リハーサル結果画面のスクリーンショットを貼り付けてください．"
        )

    trial_id = str(uuid.uuid4())[:8]
    storage.add_trial_scores(trial_id, parse_result["stages"])

    return {
        "success": True,
        "trial_id": trial_id,
        "detected_count": total_scores,
        "stages": parse_result["stages"]
    }

# 自動検知スキャナー制御API
@app.post("/api/auto_scan/start")
def start_auto_scan():
    """自動切り替え検知スキャンを開始します．"""
    scanner.start()
    return {"success": True, "status": scanner.get_status()}

@app.post("/api/auto_scan/stop")
def stop_auto_scan():
    """自動切り替え検知スキャンを停止します．"""
    scanner.stop()
    return {"success": True, "status": scanner.get_status()}

@app.get("/api/auto_scan/status")
def get_auto_scan_status():
    """自動切り替え検知スキャンの状態を取得します．"""
    return {"status": scanner.get_status()}

@app.get("/api/last_capture_image")
def get_last_capture_image():
    """直近にキャプチャされた画像ファイルを返します．"""
    if os.path.exists(LAST_CAPTURE_PATH):
        return FileResponse(LAST_CAPTURE_PATH, media_type="image/png")
    return Response(status_code=404)

@app.post("/api/capture_sample")
async def capture_sample():
    """PC版サンプル画像を取り込みます．"""
    # data フォルダに sample_result.png（リハーサル結果画面のスクショ）を置くと読み込める
    sample_path = os.path.join(DATA_DIR, "sample_result.png")
    if not os.path.exists(sample_path):
        raise HTTPException(status_code=404, detail="サンプル画像が見つかりません．")

    image = Image.open(sample_path)
    image.save(LAST_CAPTURE_PATH)

    parse_result = await ocr_parser.parse_image(image)
    trial_id = str(uuid.uuid4())[:8]

    storage.add_trial_scores(trial_id, parse_result["stages"])

    return {
        "success": True,
        "trial_id": trial_id,
        "stages": parse_result["stages"]
    }

@app.get("/api/stage_analytics")
def get_stage_analytics(
    stage: int = Query(1, ge=1, le=3, description="ステージ番号 (1, 2, 3)"),
    base_value: int = Query(300000, description="基準値"),
    bin_width: int = Query(100000, description="分布幅"),
    auto: bool = Query(False, description="範囲をデータに合わせて自動で決める")
):
    """ステージ内の3キャラ（枠1, 枠2, 枠3）の統計量および度数分布グラフデータを同時に取得します．"""
    idol_names = storage.get_all_idol_names()
    characters_data = []

    for slot in [1, 2, 3]:
        key = f"stg{stage}_slot{slot}"
        name = idol_names.get(key, f"枠{slot}")
        scores = storage.get_scores_by_stage_and_slot(stage, slot)
        stats = RehearsalAnalytics.calculate_statistics(
            scores=scores,
            target_name=name,
            base_value=base_value,
            bin_width=bin_width,
            auto=auto
        )
        characters_data.append({
            "stage": stage,
            "slot": slot,
            "key": key,
            "name": name,
            "stats": stats
        })

    return {
        "stage": stage,
        "characters": characters_data,
        "idol_names": idol_names
    }

@app.get("/api/analytics")
def get_analytics(
    target: Optional[str] = Query(None, description="全体，または stage_1，stg1_slot1 等"),
    base_value: int = Query(300000, description="度数分布の基準値"),
    bin_width: int = Query(100000, description="度数分布の幅"),
    auto: bool = Query(False, description="範囲をデータに合わせて自動で決める")
):
    """単一ターゲットの統計量と度数分布表を取得します．"""
    scores = storage.get_all_scores(target_filter=target)
    idol_names = storage.get_all_idol_names()
    target_name = idol_names.get(target, target or "全体")

    stats = RehearsalAnalytics.calculate_statistics(
        scores=scores,
        target_name=target_name,
        base_value=base_value,
        bin_width=bin_width,
        auto=auto
    )
    return {
        "stats": stats,
        "idol_names": idol_names
    }

@app.post("/api/set_idol_name")
def set_idol_name(req: SetIdolNameRequest):
    """枠のキャラ名（カスタム名）を保存します．"""
    storage.set_idol_name(req.stage, req.slot, req.name.strip())
    return {"success": True, "idol_names": storage.get_all_idol_names()}

@app.get("/api/trials")
def get_trials():
    """最近の試行データを取得します．"""
    return {"trials": storage.get_recent_trials()}

@app.get("/api/rehearsal_rows")
def rehearsal_rows(source: str = Query("db", description="db または session:<セッションID>")):
    """リハーサル分析カード用：1 回 = 3 ステージ × 3 人のスコア表．"""
    import re as _re
    idol_names = storage.get_all_idol_names()
    rows = []
    if source.startswith("session:"):
        from app.automation.session import Session
        sid = source.split(":", 1)[1]
        if not _re.fullmatch(r"[0-9A-Za-z_\-]+", sid):
            raise HTTPException(status_code=400, detail="不正なセッション ID")
        try:
            sess = Session.open(sid)
        except FileNotFoundError:
            raise HTTPException(status_code=404, detail="セッションが見つかりません")
        for r in sess.rows():
            flags = []
            for st in (1, 2, 3):
                m = _re.search(rf"S{st}:([^;]*)", r.get("flags") or "")
                bad = r["status"] == "flagged" and (m is None or any(k in m.group(1) for k in ("mismatch", "ambiguous", "error")))
                flags.append("flagged" if bad else "ok")
            rows.append({"iteration": r["iteration"], "flags": flags,
                         "scores": [[r[f"S{st}C{c}"] or 0 for c in (1, 2, 3)] for st in (1, 2, 3)]})
    else:
        trials: Dict[str, list] = {}
        order = []
        with storage.get_connection() as conn:
            for tid, stg, slot, score in conn.execute(
                    "SELECT trial_id, stage, slot, score FROM rehearsal_scores ORDER BY id"):
                if tid not in trials:
                    trials[tid] = [[0, 0, 0], [0, 0, 0], [0, 0, 0]]
                    order.append(tid)
                if 1 <= stg <= 3 and 1 <= slot <= 3:
                    trials[tid][stg - 1][slot - 1] = score
        rows = [{"trial_id": t, "flags": ["ok"] * 3, "scores": trials[t]} for t in order]
    return {"rows": rows, "idol_names": idol_names}

@app.get("/api/export_csv")
def export_csv():
    """ダッシュボードに登録されている全スコアを CSV で保存（Excel で開ける UTF-8 BOM 付き）．"""
    import csv
    import sqlite3
    from datetime import datetime as _dt
    buf = io.StringIO()
    w = csv.writer(buf)
    w.writerow(["trial_id", "created_at", "stage", "slot", "idol_name", "score"])
    with storage.get_connection() as conn:
        for r in conn.execute("SELECT trial_id, created_at, stage, slot, idol_name, score FROM rehearsal_scores ORDER BY id"):
            w.writerow(r)
    name = f"rehearsal_scores_{_dt.now().strftime('%Y%m%d_%H%M%S')}.csv"
    return Response(("\ufeff" + buf.getvalue()).encode("utf-8"), media_type="text/csv",
                    headers={"Content-Disposition": f'attachment; filename="{name}"'})

@app.post("/api/reset")
def reset_data():
    """全試行データを消去します．"""
    storage.clear_all()
    return {"success": True}

@app.get("/", response_class=HTMLResponse)
def index():
    html_path = os.path.join(STATIC_DIR, "index.html")
    if os.path.exists(html_path):
        # 更新後に古いページがブラウザに残らないようキャッシュさせない
        return FileResponse(html_path, headers={"Cache-Control": "no-store"})
    return "<h1>学マス 分析ツール準備中</h1>"

@app.get("/automation", response_class=HTMLResponse)
def automation_page():
    """自動周回の操作・ライブ集計・レビュー・設定ページ．"""
    return FileResponse(os.path.join(STATIC_DIR, "automation.html"), headers={"Cache-Control": "no-store"})
