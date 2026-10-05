"""Windows API まわり：ウィンドウ検索・クライアント領域キャプチャ・クリック送信．

Windows 以外では import だけ通るようにしてある（ロジックのテスト用）．
"""
import ctypes
import sys
import time
from typing import List, Optional, Tuple

from PIL import Image

IS_WINDOWS = sys.platform == "win32"

if IS_WINDOWS:
    from ctypes import wintypes

    user32 = ctypes.windll.user32
    gdi32 = ctypes.windll.gdi32
    shell32 = ctypes.windll.shell32

    try:
        ctypes.windll.shcore.SetProcessDpiAwareness(2)  # Per-Monitor DPI Aware
    except Exception:
        try:
            user32.SetProcessDPIAware()
        except Exception:
            pass

    WM_MOUSEMOVE = 0x0200
    WM_LBUTTONDOWN = 0x0201
    WM_LBUTTONUP = 0x0202
    MK_LBUTTON = 0x0001
    PW_CLIENTONLY = 0x1
    PW_RENDERFULLCONTENT = 0x2
    SW_RESTORE = 9

    INPUT_MOUSE = 0
    MOUSEEVENTF_LEFTDOWN = 0x0002
    MOUSEEVENTF_LEFTUP = 0x0004

    class MOUSEINPUT(ctypes.Structure):
        _fields_ = [("dx", wintypes.LONG), ("dy", wintypes.LONG), ("mouseData", wintypes.DWORD),
                    ("dwFlags", wintypes.DWORD), ("time", wintypes.DWORD),
                    ("dwExtraInfo", ctypes.POINTER(ctypes.c_ulong))]

    class INPUT(ctypes.Structure):
        class _I(ctypes.Union):
            _fields_ = [("mi", MOUSEINPUT)]
        _anonymous_ = ("i",)
        _fields_ = [("type", wintypes.DWORD), ("i", _I)]

    class BITMAPINFOHEADER(ctypes.Structure):
        _fields_ = [("biSize", wintypes.DWORD), ("biWidth", wintypes.LONG), ("biHeight", wintypes.LONG),
                    ("biPlanes", wintypes.WORD), ("biBitCount", wintypes.WORD),
                    ("biCompression", wintypes.DWORD), ("biSizeImage", wintypes.DWORD),
                    ("biXPelsPerMeter", wintypes.LONG), ("biYPelsPerMeter", wintypes.LONG),
                    ("biClrUsed", wintypes.DWORD), ("biClrImportant", wintypes.DWORD)]


def is_admin() -> bool:
    if not IS_WINDOWS:
        return False
    try:
        return bool(shell32.IsUserAnAdmin())
    except Exception:
        return False


def list_windows() -> List[Tuple[int, str]]:
    if not IS_WINDOWS:
        return []
    result = []

    @ctypes.WINFUNCTYPE(ctypes.c_bool, wintypes.HWND, wintypes.LPARAM)
    def _cb(hwnd, _):
        if user32.IsWindowVisible(hwnd):
            n = user32.GetWindowTextLengthW(hwnd)
            if n > 0:
                buf = ctypes.create_unicode_buffer(n + 1)
                user32.GetWindowTextW(hwnd, buf, n + 1)
                if buf.value.strip():
                    result.append((hwnd, buf.value))
        return True

    user32.EnumWindows(_cb, 0)
    return result


def find_window(title: str) -> Optional[int]:
    t = title.strip().lower()
    wins = list_windows()
    for hwnd, name in wins:
        if name.strip().lower() == t:
            return hwnd
    for hwnd, name in wins:
        n = name.strip().lower()
        if n.startswith(t) and not any(b in n for b in ("chrome", "edge", "visual studio", "firefox")):
            return hwnd
    return None


def window_exists(hwnd: int) -> bool:
    return IS_WINDOWS and bool(user32.IsWindow(hwnd))


def is_minimized(hwnd: int) -> bool:
    return IS_WINDOWS and bool(user32.IsIconic(hwnd))


def client_size(hwnd: int) -> Tuple[int, int]:
    r = wintypes.RECT()
    user32.GetClientRect(hwnd, ctypes.byref(r))
    return r.right - r.left, r.bottom - r.top


def client_origin(hwnd: int) -> Tuple[int, int]:
    pt = wintypes.POINT(0, 0)
    user32.ClientToScreen(hwnd, ctypes.byref(pt))
    return pt.x, pt.y


def _capture_printwindow(hwnd: int) -> Optional[Image.Image]:
    w, h = client_size(hwnd)
    if w <= 0 or h <= 0:
        return None
    hdc = user32.GetDC(hwnd)
    mdc = gdi32.CreateCompatibleDC(hdc)
    bmp = gdi32.CreateCompatibleBitmap(hdc, w, h)
    old = gdi32.SelectObject(mdc, bmp)
    try:
        ok = user32.PrintWindow(hwnd, mdc, PW_CLIENTONLY | PW_RENDERFULLCONTENT)
        if not ok:
            return None
        bi = BITMAPINFOHEADER()
        bi.biSize = ctypes.sizeof(BITMAPINFOHEADER)
        bi.biWidth, bi.biHeight, bi.biPlanes, bi.biBitCount = w, -h, 1, 32
        buf = ctypes.create_string_buffer(w * h * 4)
        gdi32.GetDIBits(mdc, bmp, 0, h, buf, ctypes.byref(bi), 0)
        return Image.frombuffer("RGBA", (w, h), buf, "raw", "BGRA", 0, 1).convert("RGB")
    finally:
        gdi32.SelectObject(mdc, old)
        gdi32.DeleteObject(bmp)
        gdi32.DeleteDC(mdc)
        user32.ReleaseDC(hwnd, hdc)


def _capture_screen(hwnd: int) -> Optional[Image.Image]:
    from PIL import ImageGrab
    w, h = client_size(hwnd)
    x, y = client_origin(hwnd)
    if w <= 0 or h <= 0:
        return None
    return ImageGrab.grab(bbox=(x, y, x + w, y + h), all_screens=True)


def _looks_blank(img: Image.Image) -> bool:
    small = img.convert("L").resize((32, 32))
    lo, hi = small.getextrema()
    return hi - lo < 6


def capture_client(hwnd: int, method: str = "auto") -> Optional[Image.Image]:
    """ゲーム画面（クライアント領域）を撮影する．"""
    if not IS_WINDOWS or not window_exists(hwnd) or is_minimized(hwnd):
        return None
    if method in ("auto", "printwindow"):
        try:
            img = _capture_printwindow(hwnd)
            if img is not None and not _looks_blank(img):
                return img
        except Exception as e:
            print(f"[capture] PrintWindow 失敗: {e}")
        if method == "printwindow":
            return None
    try:
        return _capture_screen(hwnd)
    except Exception as e:
        print(f"[capture] 画面キャプチャ失敗: {e}")
        return None


def _to_client_px(hwnd: int, rx: float, ry: float) -> Tuple[int, int]:
    w, h = client_size(hwnd)
    return int(round(rx * w)), int(round(ry * h))


def click_postmessage(hwnd: int, rx: float, ry: float):
    x, y = _to_client_px(hwnd, rx, ry)
    lp = (y << 16) | (x & 0xFFFF)
    user32.PostMessageW(hwnd, WM_MOUSEMOVE, 0, lp)
    time.sleep(0.03)
    user32.PostMessageW(hwnd, WM_LBUTTONDOWN, MK_LBUTTON, lp)
    time.sleep(0.06)
    user32.PostMessageW(hwnd, WM_LBUTTONUP, 0, lp)


def click_sendinput(hwnd: int, rx: float, ry: float):
    """実カーソルを動かしてクリックし，元の位置に戻す（ゲームを前面化する）．"""
    x, y = _to_client_px(hwnd, rx, ry)
    ox, oy = client_origin(hwnd)
    prev = wintypes.POINT()
    user32.GetCursorPos(ctypes.byref(prev))
    user32.SetForegroundWindow(hwnd)
    time.sleep(0.05)
    user32.SetCursorPos(ox + x, oy + y)
    time.sleep(0.03)
    for flag in (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP):
        inp = INPUT(type=INPUT_MOUSE)
        inp.mi = MOUSEINPUT(0, 0, 0, flag, 0, None)
        user32.SendInput(1, ctypes.byref(inp), ctypes.sizeof(INPUT))
        time.sleep(0.05)
    user32.SetCursorPos(prev.x, prev.y)


def click(hwnd: int, rx: float, ry: float, method: str = "postmessage"):
    if not IS_WINDOWS:
        raise RuntimeError("Windows でのみ動作します")
    if method == "sendinput":
        click_sendinput(hwnd, rx, ry)
    else:
        click_postmessage(hwnd, rx, ry)


def copy_image_to_clipboard(img: Image.Image) -> bool:
    try:
        import io
        import win32clipboard
        buf = io.BytesIO()
        img.convert("RGB").save(buf, "BMP")
        data = buf.getvalue()[14:]  # BMP ファイルヘッダを除いた DIB
        win32clipboard.OpenClipboard()
        try:
            win32clipboard.EmptyClipboard()
            win32clipboard.SetClipboardData(win32clipboard.CF_DIB, data)
        finally:
            win32clipboard.CloseClipboard()
        return True
    except Exception as e:
        print(f"[clipboard] コピー失敗: {e}")
        return False
