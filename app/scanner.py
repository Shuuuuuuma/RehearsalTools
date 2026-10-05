import threading
import time
import uuid
from typing import Optional, List, Dict, Any
from app.capture import WindowCapture
from app.ocr_parser import RehearsalOcrParser
from app.storage import ScoreStorage

class AutoResultScanner:
    """学マスのリハーサル結果画面を常時監視し，スコアの切り替わりを自動検知して記録するスキャナー．"""

    def __init__(self, capture_mgr: WindowCapture, ocr_parser: RehearsalOcrParser, storage: ScoreStorage):
        self.capture_mgr = capture_mgr
        self.ocr_parser = ocr_parser
        self.storage = storage

        self.is_running = False
        self.thread: Optional[threading.Thread] = None
        self.last_scanned_signature = None # 前回記録したスコアのタプル
        self.last_event_time = None
        self.last_status_message = "待機中"
        self.detected_count = 0

    def start(self):
        """自動監視スレッドを開始します．"""
        if self.is_running:
            return

        self.is_running = True
        self.last_status_message = "監視実行中（リハーサル結果画面を待機しています）"
        self.thread = threading.Thread(target=self._run_loop, daemon=True)
        self.thread.start()

    def stop(self):
        """自動監視スレッドを停止します．"""
        self.is_running = False
        self.last_status_message = "監視停止中"

    def _run_loop(self):
        """1.5秒おきに画面をチェックするループ処理．"""
        import asyncio

        loop = asyncio.new_event_loop()
        asyncio.set_event_loop(loop)

        while self.is_running:
            try:
                hwnd = self.capture_mgr.find_target_window()
                if hwnd:
                    image = self.capture_mgr.capture(hwnd)
                    if image:
                        # OCR解析
                        res = loop.run_until_complete(self.ocr_parser.parse_image(image))
                        stages = res.get("stages", [])

                        # 全ステージのスコアリストを作成
                        flat_scores = []
                        valid_stages = 0
                        for stg in stages:
                            scs = stg.get("scores", [])
                            flat_scores.extend(scs)
                            if len(scs) >= 2:
                                valid_stages += 1

                        # 少なくとも2ステージ以上（スコア計5個以上）が認識できている場合（リハーサル結果画面と判断）
                        if valid_stages >= 2 and len(flat_scores) >= 5:
                            current_sig = tuple(flat_scores)

                            # 前回のスコアと異なる（新しいリハーサル結果が出た）場合のみ自動保存
                            if current_sig != self.last_scanned_signature:
                                trial_id = str(uuid.uuid4())[:8]
                                self.storage.add_trial_scores(trial_id, stages)
                                self.last_scanned_signature = current_sig
                                self.last_event_time = time.time()
                                self.detected_count += 1
                                self.last_status_message = f"🎉 新しいリハーサル結果を自動検知して記録しました！（検出数: {len(flat_scores)}件）"
                                print(f"[AutoScanner] {self.last_status_message}")
                            else:
                                self.last_status_message = "同一の結果画面が表示されています（次のリハーサル待機中）"
                        else:
                            self.last_status_message = "結果画面待機中（リハーサル実行中）"
                else:
                    self.last_status_message = "学マスウィンドウ未検出"
            except Exception as e:
                self.last_status_message = f"監視エラー: {e}"

            time.sleep(1.5)

    def get_status(self) -> Dict[str, Any]:
        """現在の監視ステータスを返します．"""
        return {
            "is_running": self.is_running,
            "status_message": self.last_status_message,
            "detected_count": self.detected_count,
            "last_event_time": self.last_event_time
        }
