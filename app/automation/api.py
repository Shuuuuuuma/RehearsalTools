"""自動周回まわりの Web API（/api/auto/...）．"""
import base64
import io
import os
import re
from typing import Any, Dict, List, Optional

from fastapi import APIRouter, HTTPException
from fastapi.responses import FileResponse, Response
from PIL import Image
from pydantic import BaseModel

from app.automation import winapi
from app.automation.config import ConfigStore
from app.automation.detector import crop_rel
from app.automation.ocr import ResultReader
from app.automation.runner import AutomationRunner
from app.automation.session import BONUS_COLS, SCORE_COLS, TOTAL_COLS, Session, merge_sessions, suggest_mapping


class StartReq(BaseModel):
    iterations: int
    session_id: Optional[str] = None


class LineupReq(BaseModel):
    session_id: str


class MergeReq(BaseModel):
    base: str
    source: str
    mapping: Optional[List[Optional[int]]] = None


class ClickReq(BaseModel):
    button: Optional[str] = None
    x: Optional[float] = None
    y: Optional[float] = None


class ImageReq(BaseModel):
    image_base64: Optional[str] = None


class RowsReq(BaseModel):
    rows: List[Dict[str, Any]]


def _png(img: Image.Image) -> Response:
    buf = io.BytesIO()
    img.save(buf, "PNG")
    return Response(buf.getvalue(), media_type="image/png", headers={"Cache-Control": "no-store"})


def _sid(sid: str) -> Session:
    if not re.fullmatch(r"[0-9A-Za-z_\-]+", sid):
        raise HTTPException(400, "不正なセッション ID")
    try:
        return Session.open(sid)
    except FileNotFoundError:
        raise HTTPException(404, "セッションが見つかりません")


def build_router(config: ConfigStore, runner: AutomationRunner, hotkeys_info: Dict[str, Any]) -> APIRouter:
    r = APIRouter(prefix="/api/auto")

    def hwnd_or_404() -> int:
        h = winapi.find_window(config.get()["window_title"])
        if not h:
            raise HTTPException(404, "ゲームウィンドウが見つかりません")
        return h

    def screen_or_404() -> Image.Image:
        img = winapi.capture_client(hwnd_or_404(), config.get()["capture_method"])
        if img is None:
            raise HTTPException(500, "キャプチャに失敗しました（最小化されていませんか？）")
        return img

    # ---------------- 状態・実行 ----------------
    @r.get("/env")
    def env():
        cfg = config.get()
        h = winapi.find_window(cfg["window_title"])
        size = winapi.client_size(h) if h else None
        try:
            engine = runner.reader().engine.name
        except Exception as e:
            engine = f"使用不可: {e}"
        from app.automation import VERSION
        return {
            "version": VERSION,
            "is_windows": winapi.IS_WINDOWS,
            "is_admin": winapi.is_admin(),
            "window_found": bool(h),
            "client_size": size,
            "ocr_engine": engine,
            "references": {b: runner.detector.has_reference(b) for b in ("start", "skip", "end")},
            "hotkeys": hotkeys_info.get("registered", []),
            "last_iterations": runner.last_iterations,
        }

    @r.get("/status")
    def status():
        return runner.get_status()

    @r.post("/start")
    def start(req: StartReq):
        try:
            return runner.start(req.iterations, req.session_id)
        except Exception as e:
            raise HTTPException(400, str(e))

    lineup_state: Dict[str, Any] = {"screen": None}

    @r.post("/lineup_check")
    def lineup_check(req: LineupReq):
        """追加周回の前に，今の編成がセッション開始時と同じかを確認する．"""
        s = _sid(req.session_id)
        if not os.path.exists(s.start_page):
            return {"has_reference": False, "changed": []}
        screen = screen_or_404()
        if not runner.detector.check_all(screen)["start"]["visible"]:
            raise HTTPException(400, "ゲームをリハーサル画面（編成が並んでいる画面）にしてから開始してください")
        cfg = config.get()
        prev = Image.open(s.start_page).convert("RGB")
        dist = runner.detector.lineup_distance(screen, prev, cfg["regions"]["icons"])
        thr = float(cfg["lineup_threshold"])
        lineup_state["screen"] = screen
        return {"has_reference": True, "threshold": thr, "distances": [round(d, 1) for d in dist],
                "changed": [i for i, d in enumerate(dist) if d > thr]}

    @r.get("/lineup_current/{idx}")
    def lineup_current(idx: int):
        if lineup_state["screen"] is None or not 0 <= idx < 9:
            raise HTTPException(404)
        return _png(crop_rel(lineup_state["screen"], config.get()["regions"]["icons"][idx]))

    @r.post("/abort")
    def abort():
        runner.abort()
        return {"ok": True}

    @r.post("/screenshot")
    def screenshot():
        try:
            return runner.screenshot()
        except Exception as e:
            raise HTTPException(400, str(e))

    # ---------------- キャリブレーション ----------------
    @r.get("/preview")
    def preview():
        return _png(screen_or_404())

    @r.get("/detect")
    def detect():
        return runner.detector.check_all(screen_or_404())

    @r.post("/reference/{button}")
    def save_reference(button: str):
        if button not in ("start", "skip", "end", "confirm"):
            raise HTTPException(400, "button は start / skip / end / confirm")
        path = runner.detector.save_reference(button, screen_or_404())
        return {"ok": True, "path": path}

    @r.get("/reference/{button}")
    def get_reference(button: str):
        if button not in ("start", "skip", "end", "confirm"):
            raise HTTPException(400)
        p = ConfigStore.reference_path(config.get()["buttons"][button]["reference"])
        if not os.path.exists(p):
            raise HTTPException(404)
        return FileResponse(p, headers={"Cache-Control": "no-store"})

    @r.post("/click_test")
    def click_test(req: ClickReq):
        if runner.running:
            raise HTTPException(400, "自動周回中はテストできません")
        h = hwnd_or_404()
        cfg = config.get()
        if req.button:
            x, y = cfg["buttons"][req.button]["click"]
        else:
            x, y = req.x, req.y
        winapi.click(h, float(x), float(y), cfg["click_method"])
        return {"ok": True, "x": x, "y": y, "method": cfg["click_method"]}

    @r.post("/ocr_test")
    def ocr_test(req: ImageReq):
        if req.image_base64:
            data = req.image_base64.split(",", 1)[-1]
            img = Image.open(io.BytesIO(base64.b64decode(data))).convert("RGB")
        else:
            img = screen_or_404()
        try:
            res = runner.reader().read(img, config.get())
        except Exception as e:
            raise HTTPException(500, f"OCR 失敗: {e}")
        return res

    # ---------------- 設定 ----------------
    @r.get("/config")
    def get_config():
        return config.get()

    @r.post("/config")
    def set_config(patch: Dict[str, Any]):
        if runner.running:
            raise HTTPException(400, "自動周回中は設定を変更できません")
        cfg = config.update(patch)
        runner._reader = None  # OCR 設定を反映
        return cfg

    @r.post("/config/reset")
    def reset_config():
        return config.reset()

    # ---------------- セッション ----------------
    @r.get("/sessions")
    def sessions():
        return {"sessions": Session.list_all()}

    @r.get("/sessions/{sid}/rows")
    def rows(sid: str):
        return {"rows": _sid(sid).rows()}

    @r.get("/sessions/{sid}/stats")
    def stats(sid: str):
        return _sid(sid).statistics()

    @r.post("/sessions/{sid}/rows")
    def save_rows(sid: str, req: RowsReq):
        s = _sid(sid)
        old = {row["iteration"]: row for row in s.rows()}
        changed = 0
        for row in req.rows:
            it = int(row["iteration"])
            if it not in old:
                continue
            base = old[it]
            edits = False
            for k in SCORE_COLS + TOTAL_COLS + BONUS_COLS:
                if k in row:
                    v = row[k]
                    v = None if v in ("", None) else int(str(v).replace(",", ""))
                    if v != base[k]:
                        base[k] = v
                        edits = True
            if edits:
                base["status"] = "manual"
                base["recovery"] = "manual"
                changed += 1
            elif row.get("status") in ("verified",) and base["status"] != "verified":
                base["status"] = "verified"
                changed += 1
        s.save_rows(list(old.values()))
        s.write_statistics()
        return {"ok": True, "changed": changed}

    @r.post("/sessions/{sid}/verify/{it}")
    def verify(sid: str, it: int):
        s = _sid(sid)
        rows_ = s.rows()
        for row in rows_:
            if row["iteration"] == it:
                row["status"] = "verified"
        s.save_rows(rows_)
        return {"ok": True}

    @r.post("/sessions/{sid}/reocr")
    def reocr(sid: str):
        """保存済みスクショを今の読み取り方式で読み直す（手修正した行はそのまま）．"""
        if runner.running:
            raise HTTPException(400, "自動周回中は実行できません")
        s = _sid(sid)
        cfg = config.get()
        reader = runner.reader()
        old = {row["iteration"]: row for row in s.rows()}
        its = sorted(int(f[:4]) for f in os.listdir(s.shot_dir) if f[:4].isdigit() and f.endswith(".png"))
        counts: Dict[str, int] = {}
        rows = []
        for it in its:
            prev = old.get(it)
            if prev and prev.get("recovery") == "manual":
                rows.append(prev)
                counts["manual"] = counts.get("manual", 0) + 1
                continue
            res = reader.read(Image.open(s.screenshot_path(it)).convert("RGB"), cfg)
            st = res["stages"]
            row = {"iteration": it, "timestamp": prev["timestamp"] if prev else "",
                   "status": res["status"], "flags": res["flags"], "recovery": "auto"}
            for si in range(3):
                for c in range(3):
                    row[f"S{si + 1}C{c + 1}"] = st[si]["scores"][c]
                row[f"S{si + 1}_total"] = st[si]["total"]
                row[f"S{si + 1}_bonus"] = st[si]["bonus"]
            rows.append(row)
            counts[res["status"]] = counts.get(res["status"], 0) + 1
        s.save_rows(rows)
        s.write_statistics()
        try:
            s.generate_charts(cfg["regions"]["icons"])
        except Exception as e:
            print(f"[reocr] グラフ出力失敗: {e}")
        return {"ok": True, "counts": counts, "iterations": len(rows)}

    @r.post("/merge_suggest")
    def merge_suggest(req: MergeReq):
        base, source = _sid(req.base), _sid(req.source)
        if base.id == source.id:
            raise HTTPException(400, "結合先と結合元が同じです")
        return suggest_mapping(base, source, config.get()["regions"]["icons"])

    @r.post("/merge")
    def merge(req: MergeReq):
        base, source = _sid(req.base), _sid(req.source)
        if base.id == source.id:
            raise HTTPException(400, "結合先と結合元が同じです")
        if not req.mapping or len(req.mapping) != 9:
            raise HTTPException(400, "対応付け（9 枠分）が必要です")
        if not any(m is not None for m in req.mapping):
            raise HTTPException(400, "足す枠が 1 つも選ばれていません")
        for m in req.mapping:
            if m is not None and not 0 <= m < 9:
                raise HTTPException(400, "対応付けの値が不正です")
        new = merge_sessions(base, source, req.mapping, config.get()["regions"]["icons"])
        return {"session": new.id, "rows": len(new.rows())}

    @r.post("/sessions/{sid}/charts")
    def charts(sid: str):
        s = _sid(sid)
        s.write_statistics()
        files = s.generate_charts(config.get()["regions"]["icons"])
        return {"files": [os.path.basename(f) for f in files]}

    @r.get("/sessions/{sid}/charts/{name}")
    def chart_file(sid: str, name: str):
        s = _sid(sid)
        if not re.fullmatch(r"[\w\-]+\.png", name):
            raise HTTPException(400)
        p = os.path.join(s.chart_dir, name)
        if not os.path.exists(p):
            raise HTTPException(404)
        return FileResponse(p, headers={"Cache-Control": "no-store"})

    @r.get("/sessions/{sid}/download/{kind}")
    def download(sid: str, kind: str):
        """results.csv（全項目）／ rehearsal_data.csv（9 列）／ statistics.csv（統計表）をダウンロード．"""
        s = _sid(sid)
        if kind == "results":
            p, name = s.results_csv, f"{sid}_results.csv"
        elif kind == "raw":
            p, name = s.raw_csv, f"{sid}_rehearsal_data.csv"
        elif kind == "stats":
            import csv as _csv
            st = s.statistics()
            p, name = os.path.join(s.dir, "statistics.csv"), f"{sid}_statistics.csv"
            keys = [("n", "N"), ("avg", "平均"), ("median", "中央値"), ("max", "最大"), ("min", "最小"),
                    ("q1", "Q1"), ("q3", "Q3"), ("std", "標準偏差")]
            cols = list(st["columns"].items()) + [(f"{k}_total", v) for k, v in st["stage_totals"].items()] + \
                [("3stage_total", st["grand_total"])]
            with open(p, "w", newline="", encoding="utf-8-sig") as f:
                w = _csv.writer(f)
                w.writerow([""] + [c for c, _ in cols])
                for k, label in keys:
                    w.writerow([label] + ["" if d[k] is None else d[k] for _, d in cols])
        else:
            raise HTTPException(400, "kind は results / raw / stats")
        if not os.path.exists(p):
            raise HTTPException(404, "まだデータがありません")
        return FileResponse(p, filename=name, media_type="text/csv")

    @r.get("/sessions/{sid}/screenshot/{it}")
    def shot(sid: str, it: int):
        p = _sid(sid).screenshot_path(it)
        if not os.path.exists(p):
            raise HTTPException(404)
        return FileResponse(p)

    @r.get("/sessions/{sid}/crop/{it}/{stage}")
    def crop(sid: str, it: int, stage: int):
        p = _sid(sid).screenshot_path(it)
        if not os.path.exists(p) or stage not in (1, 2, 3):
            raise HTTPException(404)
        img = Image.open(p).convert("RGB")
        return _png(ResultReader.stage_crop(img, config.get(), stage - 1))

    @r.get("/sessions/{sid}/icon/{idx}")
    def icon(sid: str, idx: int):
        s = _sid(sid)
        if not os.path.exists(s.start_page) or not 0 <= idx < 9:
            raise HTTPException(404)
        img = Image.open(s.start_page).convert("RGB")
        return _png(crop_rel(img, config.get()["regions"]["icons"][idx]))

    @r.post("/sessions/{sid}/open")
    def open_folder(sid: str):
        s = _sid(sid)
        if winapi.IS_WINDOWS:
            os.startfile(s.dir)  # type: ignore[attr-defined]
        return {"path": s.dir}

    return r
