"""グローバルホットキー（ツールのウィンドウが前面でなくても効く）．

  Ctrl+Shift+A : 自動周回を開始（前回の周回数）
  Ctrl+Shift+Q : 自動周回を中断
  Ctrl+Shift+S : ゲーム画面のスクリーンショット（保存＋クリップボード）
"""
import ctypes
import threading
from typing import Callable, Dict

from app.automation.winapi import IS_WINDOWS

MOD_SHIFT, MOD_CONTROL, MOD_NOREPEAT = 0x4, 0x2, 0x4000
WM_HOTKEY = 0x0312


class HotkeyThread(threading.Thread):
    def __init__(self, actions: Dict[str, Callable[[], None]]):
        super().__init__(daemon=True)
        # キー文字 → 処理
        self.actions = actions
        self.registered = []

    def run(self):
        if not IS_WINDOWS:
            return
        from ctypes import wintypes
        user32 = ctypes.windll.user32
        ids = {}
        for i, key in enumerate(self.actions, start=1):
            vk = ord(key.upper())
            if user32.RegisterHotKey(None, i, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, vk):
                ids[i] = key
                self.registered.append(f"Ctrl+Shift+{key.upper()}")
            else:
                print(f"[hotkey] Ctrl+Shift+{key.upper()} は他のアプリが使用中のため登録できませんでした")
        msg = wintypes.MSG()
        while user32.GetMessageW(ctypes.byref(msg), None, 0, 0) != 0:
            if msg.message == WM_HOTKEY and msg.wParam in ids:
                fn = self.actions[ids[msg.wParam]]
                threading.Thread(target=self._safe, args=(fn,), daemon=True).start()

    @staticmethod
    def _safe(fn):
        try:
            fn()
        except Exception as e:
            print(f"[hotkey] {e}")
