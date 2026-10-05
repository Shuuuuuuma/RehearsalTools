import ctypes
from ctypes import wintypes
from PIL import Image, ImageGrab
import os
from typing import Optional, Dict, Any, List

user32 = ctypes.windll.user32
shcore = ctypes.windll.shcore

WINSTA_ALL = 0x37F
DESKTOP_ALL = 0x1FF

def init_dpi_and_desktop():
    """高DPI対応とWindows対話型デスクトップ（WinSta0\\default）へのアタッチを行います．"""
    # 高DPIスケーリングによる座標ズレを防止
    try:
        shcore.SetProcessDpiAwareness(2) # PROCESS_PER_MONITOR_DPI_AWARE
    except Exception:
        try:
            user32.SetProcessDPIAware()
        except Exception:
            pass

    # 対話型デスクトップにアタッチ
    try:
        hwinsta = user32.OpenWindowStationW("WinSta0", False, WINSTA_ALL)
        if hwinsta:
            user32.SetProcessWindowStation(hwinsta)
            hdesk = user32.OpenDesktopW("default", 0, False, DESKTOP_ALL)
            if hdesk:
                user32.SetThreadDesktop(hdesk)
    except Exception as e:
        print(f"デスクトップ初期化警告: {e}")

class WindowCapture:
    """学マスのPC版ゲームウィンドウを高DPI対応で検出し，クライアント領域をキャプチャするクラス．"""

    def __init__(self, target_title: str = "gakumas"):
        self.target_title = target_title
        init_dpi_and_desktop()

    def list_visible_windows(self) -> List[Dict[str, Any]]:
        """現在表示されているウィンドウの一覧を取得します．"""
        init_dpi_and_desktop()
        windows = []

        def enum_windows_proc(hwnd, lParam):
            if user32.IsWindowVisible(hwnd):
                length = user32.GetWindowTextLengthW(hwnd)
                if length > 0:
                    buff = ctypes.create_unicode_buffer(length + 1)
                    user32.GetWindowTextW(hwnd, buff, length + 1)
                    title = buff.value
                    if title.strip():
                        windows.append({
                            "hwnd": hwnd,
                            "title": title
                        })
            return True

        EnumWindowsProc = ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
        user32.EnumWindows(EnumWindowsProc(enum_windows_proc), 0)
        return windows

    def find_target_window(self) -> Optional[int]:
        """対象のウィンドウハンドル（HWND）を完全一致優先で検索します．"""
        init_dpi_and_desktop()
        windows = self.list_visible_windows()

        # 優先度1: 完全一致 "gakumas"
        for w in windows:
            if w["title"].strip().lower() == self.target_title.lower():
                return w["hwnd"]

        # 優先度2: 前方一致 "gakumas"
        for w in windows:
            if w["title"].strip().lower().startswith(self.target_title.lower()):
                return w["hwnd"]

        # 優先度3: 部分一致（ブラウザや開発ツールを除外）
        for w in windows:
            t = w["title"].lower()
            if self.target_title.lower() in t and "chrome" not in t and "visual studio" not in t and "edge" not in t:
                return w["hwnd"]

        return None

    def get_client_rect_screen(self, hwnd: int) -> Optional[tuple]:
        """タイトルバーや枠線を除いたゲーム描画領域の物理画面絶対座標を取得します．"""
        init_dpi_and_desktop()
        client_rect = wintypes.RECT()
        if not user32.GetClientRect(hwnd, ctypes.byref(client_rect)):
            return None

        pt_top_left = wintypes.POINT(client_rect.left, client_rect.top)
        pt_bottom_right = wintypes.POINT(client_rect.right, client_rect.bottom)

        user32.ClientToScreen(hwnd, ctypes.byref(pt_top_left))
        user32.ClientToScreen(hwnd, ctypes.byref(pt_bottom_right))

        left = pt_top_left.x
        top = pt_top_left.y
        right = pt_bottom_right.x
        bottom = pt_bottom_right.y

        if right - left <= 0 or bottom - top <= 0:
            return None

        return (left, top, right, bottom)

    def capture(self, hwnd: Optional[int] = None) -> Optional[Image.Image]:
        """指定したウィンドウ（またはgakumas）のクライアント領域を撮影します．"""
        init_dpi_and_desktop()
        if hwnd is None:
            hwnd = self.find_target_window()

        if not hwnd:
            return None

        bbox = self.get_client_rect_screen(hwnd)
        if not bbox:
            return None

        try:
            image = ImageGrab.grab(bbox=bbox)
            return image
        except Exception as e:
            print(f"キャプチャエラー: {e}")
            return None
