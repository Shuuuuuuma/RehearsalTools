"""自動周回の本体（状態機械）と OCR ワーカー．

1 周の流れ：
  リハーサル画面待ち →「開始する」クリック → 対戦準備画面で「スキップ」が明るくなるのを待つ
  →「スキップ」クリック → 結果画面（「終了」ボタン）待ち → 撮影 → OCR キューへ →「終了」クリック
クリックは押せたかを画面の変化で確認し，変化がなければ再クリックする．
"""
import os
import queue
import threading
import time
import traceback
from collections import deque
from datetime import datetime
from typing import Any, Callable, Dict, Optional

from PIL import Image

from app.automation import winapi
from app.automation.config import SCREENSHOT_DIR, ConfigStore
from app.automation.detector import ScreenDetector
from app.automation.ocr import OcrEngine, ResultReader
from app.automation.session import Session


class WaitTimeout(Exception):
    pass


class Aborted(Exception):
    pass


class WindowLost(Exception):
    pass


STATE_LABELS = {
    "idle": "待機中",
    "wait_page": "リハーサル画面待ち",
    "click_start": "「開始する」をクリック",
    "wait_skip": "対戦準備中（スキップ待ち）",
    "click_skip": "「スキップ」をクリック",
    "confirm": "確認ダイアログ",
    "wait_result": "結果画面待ち",
    "capture": "結果を撮影",
    "click_end": "「終了」をクリック",
    "finishing": "集計中",
}


class AutomationRunner:
    def __init__(self, config: ConfigStore, on_result: Optional[Callable[[str, int, Dict[str, Any]], None]] = None):
        self.config = config
        self.detector = ScreenDetector(config)
        self.on_result = on_result
        self._reader: Optional[ResultReader] = None
        self._thread: Optional[threading.Thread] = None
        self._abort = threading.Event()
        self._lock = threading.Lock()
        self._log = deque(maxlen=300)
        self.last_iterations = 10
        self.status: Dict[str, Any] = self._blank_status()

    # ------------------------------------------------------------------ 公開 API
    def reader(self) -> ResultReader:
        if self._reader is None:
            ocfg = self.config.get()["ocr"]
            self._reader = ResultReader(OcrEngine(ocfg.get("engine", "auto"), ocfg.get("lang", "ja")))
        return self._reader

    @property
    def running(self) -> bool:
        return self._thread is not None and self._thread.is_alive()

    def start(self, iterations: int, session_id: Optional[str] = None) -> Dict[str, Any]:
        if self.running:
            raise RuntimeError("すでに実行中です")
        if iterations < 1:
            raise ValueError("周回数は 1 以上にしてください")
        self.reader()  # OCR が使えるか先に確認
        self.last_iterations = iterations
        self._abort.clear()
        self.status = self._blank_status()
        self.status.update(running=True, target=iterations, started_at=time.time())
        self._thread = threading.Thread(target=self._run, args=(iterations, session_id), daemon=True)
        self._thread.start()
        return self.get_status()

    def abort(self):
        if self.running:
            self._abort.set()
            self._logline("中断を要求しました")

    def get_status(self) -> Dict[str, Any]:
        with self._lock:
            st = dict(self.status)
        st["state_label"] = STATE_LABELS.get(st["state"], st["state"])
        times = st.pop("_iter_times", [])
        st["avg_iteration_sec"] = round(sum(times) / len(times), 1) if times else None
        remaining = max(0, st["target"] - st["completed"])
        st["eta_sec"] = round(remaining * st["avg_iteration_sec"]) if times and st["running"] else None
        st["elapsed_sec"] = round(time.time() - st["started_at"]) if st["started_at"] else None
        st["log"] = list(self._log)[-80:]
        return st

    def screenshot(self) -> Dict[str, Any]:
        """ゲーム画面を撮って保存し，クリップボードにもコピーする（Ctrl+Shift+S）．"""
        hwnd = winapi.find_window(self.config.get()["window_title"])
        if not hwnd:
            raise RuntimeError("ゲームウィンドウが見つかりません")
        img = winapi.capture_client(hwnd, self.config.get()["capture_method"])
        if img is None:
            raise RuntimeError("キャプチャに失敗しました")
        os.makedirs(SCREENSHOT_DIR, exist_ok=True)
        path = os.path.join(SCREENSHOT_DIR, datetime.now().strftime("%Y%m%d_%H%M%S_%f")[:-3] + ".png")
        img.save(path)
        copied = winapi.copy_image_to_clipboard(img)
        self._logline(f"スクリーンショット保存: {os.path.basename(path)}" + ("（クリップボードにもコピー）" if copied else ""))
        return {"path": path, "clipboard": copied}

    # ------------------------------------------------------------------ 内部
    @staticmethod
    def _blank_status() -> Dict[str, Any]:
        return {"running": False, "state": "idle", "message": "", "session": None, "target": 0,
                "completed": 0, "current": 0, "started_at": None, "error": None,
                "last_result": None, "ocr_pending": 0, "_iter_times": []}

    def _set(self, **kw):
        with self._lock:
            self.status.update(kw)

    def _logline(self, msg: str):
        line = f"{datetime.now().strftime('%H:%M:%S')} {msg}"
        self._log.append(line)
        print(f"[auto] {line}")

    def _sleep(self, sec: float):
        if self._abort.wait(sec):
            raise Aborted()

    def _grab(self, hwnd: int) -> Image.Image:
        for _ in range(10):
            if not winapi.window_exists(hwnd):
                raise WindowLost()
            img = winapi.capture_client(hwnd, self.config.get()["capture_method"])
            if img is not None:
                self._last_screen = img
                return img
            self._sleep(0.3)
        raise RuntimeError("キャプチャに失敗し続けています（ウィンドウが最小化されていませんか？）")

    def _click(self, hwnd: int, button: str):
        cfg = self.config.get()
        x, y = cfg["buttons"][button]["click"]
        winapi.click(hwnd, x, y, cfg["click_method"])

    def _wait(self, hwnd: int, cond: Callable[[Dict[str, Dict[str, Any]]], bool], timeout_ms: int,
              confirm: int, limit_name: str, on_poll: Optional[Callable[[Dict[str, Any]], None]] = None) -> Image.Image:
        cfg = self.config.get()
        interval = cfg["poll_interval_ms"] / 1000.0
        deadline = time.time() + timeout_ms / 1000.0
        hits = 0
        while True:
            screen = self._grab(hwnd)
            checks = self.detector.check_all(screen)
            if cond(checks):
                hits += 1
                if hits >= confirm:
                    return screen
            else:
                hits = 0
                if on_poll:
                    on_poll(checks)
            if time.time() > deadline:
                raise WaitTimeout(limit_name)
            self._sleep(interval)

    def _wait_prepared(self, hwnd: int) -> Dict[str, Dict[str, Any]]:
        """対戦準備画面で「コンテスト準備中...」が消えてスキップが押せる状態になるまで待つ．

        準備中の表示が見えている間はタイムアウトしない（PC が遅くても止まらない）．
        loading_timeout_ms は「準備中もスキップも見えない」状態が続いた場合だけに効く．
        """
        cfg = self.config.get()
        interval = cfg["poll_interval_ms"] / 1000.0
        idle_limit = cfg["loading_timeout_ms"] / 1000.0
        prep_limit = cfg.get("prepare_timeout_ms", 0) / 1000.0
        last_seen = time.time()      # 最後に「準備中」か「スキップ」を見た時刻
        prep_started = None
        hits = 0
        last_confirm = 0.0
        retry_iv = cfg["click_retry_interval_ms"] / 1000.0
        while True:
            checks = self.detector.check_all(self._grab(hwnd))
            now = time.time()
            if checks["confirm"]["visible"]:
                # 空き枠がある編成：「メモリー未編成」の確認ダイアログ →「開始する」
                hits = 0
                last_seen = now
                if now - last_confirm >= retry_iv:
                    if last_confirm == 0.0:
                        self._logline("「メモリー未編成」の確認 →「開始する」をクリック")
                    self._click(hwnd, "confirm")
                    last_confirm = now
            elif checks["preparing"]["visible"]:
                hits = 0
                last_seen = now
                if prep_started is None:
                    prep_started = now
                    self._set(message=self.status["message"].split("｜")[0] + "｜コンテスト準備中…")
                if prep_limit > 0 and now - prep_started > prep_limit:
                    raise WaitTimeout("prepare_timeout_ms")
            elif checks["skip"]["visible"] or (checks["end"]["visible"] and not checks["start"]["visible"]):
                hits += 1
                last_seen = now
                if hits >= 2:
                    if prep_started is not None:
                        self._logline(f"準備完了（{now - prep_started:.1f} 秒）")
                    self._set(message=self.status["message"].split("｜")[0])
                    return checks
            else:
                hits = 0
                if now - last_seen > idle_limit:
                    raise WaitTimeout("loading_timeout_ms")
            self._sleep(interval)

    def _click_until(self, hwnd: int, button: str, gone: Callable[[Dict[str, Dict[str, Any]]], bool]):
        """ボタンを押し，画面が変わる（gone が真）まで間隔をあけて押し直す．"""
        cfg = self.config.get()
        retries = int(cfg["max_click_retries"])
        retry_iv = cfg["click_retry_interval_ms"] / 1000.0
        interval = cfg["poll_interval_ms"] / 1000.0
        self._click(hwnd, button)
        clicks, last = 1, time.time()
        ok = 0
        while True:
            self._sleep(interval)
            checks = self.detector.check_all(self._grab(hwnd))
            if gone(checks):
                ok += 1
                if ok >= 2:
                    return
                continue
            ok = 0
            if time.time() - last >= retry_iv:
                if clicks > retries:
                    raise RuntimeError(f"「{button}」を {clicks} 回押しても画面が変わりません（クリック方式の設定を確認してください）")
                self._logline(f"「{button}」再クリック（{clicks + 1} 回目）")
                self._click(hwnd, button)
                clicks += 1
                last = time.time()

    def _run(self, iterations: int, session_id: Optional[str]):
        cfg = self.config.get()
        ocr_q: "queue.Queue" = queue.Queue()
        session: Optional[Session] = None
        worker: Optional[threading.Thread] = None
        done = 0
        try:
            hwnd = winapi.find_window(cfg["window_title"])
            if not hwnd:
                raise RuntimeError(f"ゲームウィンドウ「{cfg['window_title']}」が見つかりません")
            session = Session.open(session_id) if session_id else Session.create()
            self._set(session=session.id)
            self._logline(f"開始: {iterations} 周（セッション {session.id}{'・追加' if session_id else ''}）")
            worker = threading.Thread(target=self._ocr_worker, args=(session, ocr_q), daemon=True)
            worker.start()

            for n in range(iterations):
                it = session.next_iteration()
                self._set(current=n + 1, message=f"{n + 1}/{iterations} 周目（通し番号 {it}）")
                t0 = time.time()
                self._one_iteration(hwnd, session, it, first=(n == 0), ocr_q=ocr_q)
                done += 1
                with self._lock:
                    self.status["completed"] = done
                    self.status["_iter_times"].append(time.time() - t0)
            self._logline(f"完了: {done} 周")
        except Aborted:
            self._logline(f"中断しました（{done} 周完了）")
            self._set(error=None, message=f"中断（{done} 周完了）")
        except WaitTimeout as e:
            msg = {"page_timeout_ms": "リハーサル画面が {} 秒以内に検出できず",
                   "loading_timeout_ms": "開始後 {} 秒間，「準備中」表示もスキップボタンも検出できず",
                   "prepare_timeout_ms": "「コンテスト準備中」が {} 秒以上続いたため",
                   "result_timeout_ms": "結果画面が {} 秒以内に検出できず"}.get(str(e), str(e) + " {}")
            text = msg.format(cfg[str(e)] // 1000) + f"停止しました（設定の {e} / 参照画像を確認）"
            # 止まった瞬間の画面を残す（原因調査用）
            if session is not None and getattr(self, "_last_screen", None) is not None:
                try:
                    p = os.path.join(session.dir, f"stopped_{datetime.now().strftime('%H%M%S')}.png")
                    self._last_screen.save(p)
                    checks = self.detector.check_all(self._last_screen)
                    text += f"　停止時の画面: {os.path.basename(p)}"
                    self._logline("停止時の判定: " + ", ".join(
                        f"{k}={'○' if v['visible'] else '×'}" for k, v in checks.items()))
                except Exception as ex:
                    print(f"[auto] 停止画面の保存失敗: {ex}")
            self._logline(text)
            self._set(error=text)
        except WindowLost:
            self._logline("ゲームウィンドウが閉じられたため停止しました")
            self._set(error="ゲームウィンドウが閉じられました")
        except Exception as e:
            traceback.print_exc()
            self._logline(f"エラーで停止: {e}")
            self._set(error=str(e))
        finally:
            self._set(state="finishing")
            if worker is not None:
                ocr_q.put(None)
                worker.join(timeout=120)
            if session is not None:
                try:
                    session.write_statistics()
                    session.generate_charts(self.config.get()["regions"]["icons"])
                    self._logline("統計・グラフを出力しました")
                except Exception as e:
                    self._logline(f"グラフ出力に失敗: {e}")
            self._set(running=False, state="idle")

    def _one_iteration(self, hwnd: int, session: Session, it: int, first: bool, ocr_q: "queue.Queue"):
        cfg = self.config.get()
        confirm = int(cfg["confirm_count"])

        # 1) リハーサル画面待ち（結果画面のまま止まっていたら「終了」を押す）
        self._set(state="wait_page")

        def leftover(checks):
            if checks["end"]["visible"] and not checks["skip"]["visible"]:
                self._logline("結果画面が残っていたため「終了」をクリック")
                self._click(hwnd, "end")
                self._sleep(1.0)

        screen = self._wait(hwnd, lambda c: c["start"]["visible"], cfg["page_timeout_ms"], confirm,
                            "page_timeout_ms", on_poll=leftover)

        # 2) 初回だけ開始画面を保存／追加周回なら編成が同じか確認
        if first:
            if os.path.exists(session.start_page):
                prev = Image.open(session.start_page).convert("RGB")
                dist = ScreenDetector.lineup_distance(screen, prev, cfg["regions"]["icons"])
                worst = max(dist)
                self._logline(f"編成チェック: 最大差 {worst:.1f}（しきい値 {cfg['lineup_threshold']}）")
                if worst > float(cfg["lineup_threshold"]):
                    bad = [f"S{i // 3 + 1}C{i % 3 + 1}" for i, d in enumerate(dist) if d > cfg["lineup_threshold"]]
                    raise RuntimeError(f"編成がセッション開始時と違います（{', '.join(bad)}）．"
                                       "新しいセッションで周回して，あとで「セッション」タブの結合でまとめてください")
            else:
                screen.save(session.start_page)

        # 3) 開始
        self._set(state="click_start")
        self._click_until(hwnd, "start", lambda c: not c["start"]["visible"])

        # 4) 対戦準備 → スキップ
        self._set(state="wait_skip")
        checks = self._wait_prepared(hwnd)
        if checks["skip"]["visible"]:
            self._set(state="click_skip")
            self._click_until(hwnd, "skip", lambda c: not c["skip"]["visible"])

        # 5) 結果画面
        self._set(state="wait_result")
        self._wait(hwnd, lambda c: c["end"]["visible"] and not c["skip"]["visible"] and not c["start"]["visible"],
                   cfg["result_timeout_ms"], confirm, "result_timeout_ms")
        self._set(state="capture")
        self._sleep(cfg["capture_delay_ms"] / 1000.0)
        shot = self._grab(hwnd)
        path = session.screenshot_path(it)
        shot.save(path)
        ocr_q.put((it, path))
        with self._lock:
            self.status["ocr_pending"] = ocr_q.qsize()

        # 6) 終了
        self._set(state="click_end")
        self._click_until(hwnd, "end", lambda c: not c["end"]["visible"])

    def _ocr_worker(self, session: Session, q: "queue.Queue"):
        reader = self.reader()
        while True:
            item = q.get()
            if item is None:
                break
            it, path = item
            try:
                img = Image.open(path).convert("RGB")
                res = reader.read(img, self.config.get())
                session.append_result(it, res)
                totals = [s["total"] for s in res["stages"]]
                self._logline(f"#{it} OCR {res['status']}  合計 {', '.join(f'{t:,}' if t else '?' for t in totals)}"
                              + (f"  [{res['flags']}]" if res["flags"] else ""))
                self._set(last_result={"iteration": it, **{k: res[k] for k in ("status", "flags")},
                                       "stages": [{k: s[k] for k in ("scores", "total", "bonus", "status")} for s in res["stages"]]})
                if self.on_result:
                    try:
                        self.on_result(session.id, it, res)
                    except Exception as e:
                        print(f"[auto] on_result エラー: {e}")
            except Exception as e:
                traceback.print_exc()
                self._logline(f"#{it} OCR 失敗: {e}")
                session.append_result(it, {"stages": [{"scores": [None] * 3, "total": None, "bonus": None}] * 3,
                                           "status": "flagged", "flags": f"error:{e}"})
            with self._lock:
                self.status["ocr_pending"] = q.qsize()
