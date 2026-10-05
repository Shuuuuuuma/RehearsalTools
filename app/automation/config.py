"""自動周回ツールの設定（automation_config.json）の読み書き．

座標はすべて「ゲームウィンドウのクライアント領域に対する比率（0.0〜1.0）」で持つ．
領域は [x, y, w, h]，クリック位置は [x, y]．
"""
import copy
import json
import os
import threading
from typing import Any, Dict

from app.paths import DATA_ROOT

BASE_DIR = DATA_ROOT
CONFIG_PATH = os.path.join(BASE_DIR, "automation_config.json")
REFERENCE_DIR = os.path.join(BASE_DIR, "references")
SESSIONS_DIR = os.path.join(BASE_DIR, "sessions")
SCREENSHOT_DIR = os.path.join(BASE_DIR, "data", "hotkey_screenshots")


def _stage_rows(y0: float, h: float, x: float, w: float, pitch: float = 0.252):
    return [[x, round(y0 + pitch * i, 4), w, h] for i in range(3)]


def _icon_grid():
    # リハーサル開始画面（編成が並ぶ画面）の 9 枠のアイコン位置
    xs = [0.440, 0.573, 0.707]
    ys = [0.438, 0.574, 0.710]
    return [[x, y, 0.118, 0.084] for y in ys for x in xs]


DEFAULT_CONFIG: Dict[str, Any] = {
    "window_title": "gakumas",
    # auto: PrintWindow（隠れていても撮れる）→ 失敗したら画面から切り出し
    "capture_method": "auto",          # auto | printwindow | screen
    # postmessage: マウスを動かさない / sendinput: 実カーソルでクリック（前面化が必要）
    "click_method": "postmessage",     # postmessage | sendinput
    "buttons": {
        "start": {
            "click": [0.49, 0.847],
            "region": [0.36, 0.830, 0.26, 0.035],
            "reference": "start.png",
        },
        "skip": {
            # 対戦準備画面の「スキップ」．準備中は画面が暗いので明るさでも判定する
            "click": [0.81, 0.822],
            "region": [0.735, 0.81, 0.18, 0.025],
            "reference": "skip.png",
            "brightness_min": 85.0,
        },
        # 空き枠がある編成で出る「メモリー未編成」確認ダイアログの「開始する」
        "confirm": {
            "click": [0.69, 0.905],
            "region": [0.60, 0.89, 0.22, 0.03],
            "cancel_region": [0.15, 0.89, 0.28, 0.03],   # 左の白い「キャンセル」ボタン（誤判定防止）
            "reference": "confirm.png",
        },
        "end": {
            "click": [0.50, 0.905],
            "region": [0.40, 0.886, 0.20, 0.038],
            "reference": "end.png",
        },
    },
    # 対戦準備画面右下の「コンテスト準備中...」（白文字）．これが出ている間はスキップを押さずに待つ
    "preparing": {
        "region": [0.70, 0.912, 0.25, 0.03],
        "white_min": 0.05,             # 領域内の白画素の割合がこれ以上なら「準備中」
    },
    "match_threshold": 0.85,           # 参照画像との類似度（0〜1）
    "confirm_count": 3,                # 連続一致回数でボタン検出を確定
    "poll_interval_ms": 250,
    "page_timeout_ms": 30000,          # リハーサル画面待ち
    "loading_timeout_ms": 60000,       # 開始後，「準備中」表示もスキップも見えない状態が続く上限
    "prepare_timeout_ms": 0,           # 「準備中」表示が続く上限（0 = 無制限で待つ）
    "result_timeout_ms": 30000,        # スキップ→結果画面まで
    "capture_delay_ms": 500,           # 結果画面確定から撮影までの待ち
    "max_click_retries": 5,
    "click_retry_interval_ms": 1200,
    "ocr": {
        "engine": "auto",              # auto(=template) | template | winocr | tesseract
        "lang": "ja",
        "score_threshold": 190,        # 白文字二値化しきい値（各アイドルスコア）
        "total_threshold": 210,        # 白文字二値化しきい値（ステージ合計）
        "bonus_blue_min": 190,         # ボーナス（青文字）判定
        "bonus_margin": 30,
        "scale": 3,
    },
    "regions": {
        "score": _stage_rows(0.178, 0.022, 0.28, 0.44),
        # 3 人分の列の境界（クライアント比率の x）
        "score_columns": [0.28, 0.433, 0.577, 0.72],
        "total": _stage_rows(0.137, 0.036, 0.30, 0.40),
        # 1 位のアイドルの上に出る吹き出し．1 位の列で位置が変わるので横幅いっぱい
        "bonus": _stage_rows(0.200, 0.024, 0.28, 0.44),
        "icons": _icon_grid(),
    },
    "lineup_threshold": 20.0,
}


def _deep_merge(base: Dict[str, Any], override: Dict[str, Any]) -> Dict[str, Any]:
    out = copy.deepcopy(base)
    for k, v in (override or {}).items():
        if isinstance(v, dict) and isinstance(out.get(k), dict):
            out[k] = _deep_merge(out[k], v)
        else:
            out[k] = v
    return out


class ConfigStore:
    def __init__(self, path: str = CONFIG_PATH):
        self.path = path
        self._lock = threading.Lock()
        self._data = self._load()
        os.makedirs(REFERENCE_DIR, exist_ok=True)
        os.makedirs(SESSIONS_DIR, exist_ok=True)

    def _load(self) -> Dict[str, Any]:
        if os.path.exists(self.path):
            try:
                with open(self.path, "r", encoding="utf-8") as f:
                    return _deep_merge(DEFAULT_CONFIG, json.load(f))
            except Exception as e:
                print(f"[config] 読み込み失敗のため既定値を使用: {e}")
        data = copy.deepcopy(DEFAULT_CONFIG)
        self._write(data)
        return data

    def _write(self, data: Dict[str, Any]):
        tmp = self.path + ".tmp"
        with open(tmp, "w", encoding="utf-8") as f:
            json.dump(data, f, ensure_ascii=False, indent=2)
        os.replace(tmp, self.path)

    def get(self) -> Dict[str, Any]:
        with self._lock:
            return copy.deepcopy(self._data)

    def update(self, patch: Dict[str, Any]) -> Dict[str, Any]:
        with self._lock:
            self._data = _deep_merge(self._data, patch)
            self._write(self._data)
            return copy.deepcopy(self._data)

    def reset(self) -> Dict[str, Any]:
        with self._lock:
            self._data = copy.deepcopy(DEFAULT_CONFIG)
            self._write(self._data)
            return copy.deepcopy(self._data)

    @staticmethod
    def reference_path(name: str) -> str:
        return os.path.join(REFERENCE_DIR, name)
