//! Windows API まわり：ウィンドウ検索・クライアント領域キャプチャ・クリック送信・クリップボード．

use std::ffi::c_void;
use std::thread::sleep;
use std::time::Duration;

use image::{imageops::FilterType, RgbImage};
use windows::core::BOOL;
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BitBlt, ClientToScreen, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetDIBits,
    ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HBITMAP, HDC, SRCCOPY,
};
use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Ole::CF_DIB;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEINPUT, MOUSE_EVENT_FLAGS,
};
use windows::Win32::UI::Shell::IsUserAnAdmin;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClientRect, GetCursorPos, GetWindowTextLengthW, GetWindowTextW, IsIconic, IsWindow,
    IsWindowVisible, PostMessageW, SetCursorPos, SetForegroundWindow, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
};

use crate::imgproc;

/// 画面とやりとりする時はウィンドウハンドルを整数で持つ（JSON にそのまま出せるように）．
pub type Hwnd = isize;

fn hwnd(h: Hwnd) -> HWND {
    HWND(h as *mut c_void)
}

pub fn is_admin() -> bool {
    unsafe { IsUserAnAdmin().as_bool() }
}

unsafe extern "system" fn enum_cb(h: HWND, lp: LPARAM) -> BOOL {
    let out = &mut *(lp.0 as *mut Vec<(Hwnd, String)>);
    if IsWindowVisible(h).as_bool() {
        let n = GetWindowTextLengthW(h);
        if n > 0 {
            let mut buf = vec![0u16; n as usize + 1];
            let len = GetWindowTextW(h, &mut buf);
            let title = String::from_utf16_lossy(&buf[..len.max(0) as usize]);
            if !title.trim().is_empty() {
                out.push((h.0 as Hwnd, title));
            }
        }
    }
    BOOL(1)
}

pub fn list_windows() -> Vec<(Hwnd, String)> {
    let mut out: Vec<(Hwnd, String)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_cb), LPARAM(&mut out as *mut _ as isize));
    }
    out
}

pub fn find_window(title: &str) -> Option<Hwnd> {
    let t = title.trim().to_lowercase();
    let wins = list_windows();
    if let Some((h, _)) = wins.iter().find(|(_, n)| n.trim().to_lowercase() == t) {
        return Some(*h);
    }
    wins.iter()
        .find(|(_, n)| {
            let n = n.trim().to_lowercase();
            n.starts_with(&t) && !["chrome", "edge", "visual studio", "firefox"].iter().any(|b| n.contains(b))
        })
        .map(|(h, _)| *h)
}

pub fn window_exists(h: Hwnd) -> bool {
    unsafe { IsWindow(Some(hwnd(h))).as_bool() }
}

pub fn is_minimized(h: Hwnd) -> bool {
    unsafe { IsIconic(hwnd(h)).as_bool() }
}

pub fn client_size(h: Hwnd) -> (i32, i32) {
    let mut r = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd(h), &mut r);
    }
    (r.right - r.left, r.bottom - r.top)
}

pub fn client_origin(h: Hwnd) -> (i32, i32) {
    let mut pt = POINT { x: 0, y: 0 };
    unsafe {
        let _ = ClientToScreen(hwnd(h), &mut pt);
    }
    (pt.x, pt.y)
}

/// メモリ DC に描かれたビットマップを RGB 画像として取り出す．
unsafe fn read_bitmap(mdc: HDC, bmp: HBITMAP, w: i32, h: i32) -> Option<RgbImage> {
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut buf = vec![0u8; (w * h * 4) as usize];
    let lines = GetDIBits(mdc, bmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
    if lines == 0 {
        return None;
    }
    let mut img = RgbImage::new(w as u32, h as u32);
    for (p, px) in img.pixels_mut().zip(buf.chunks_exact(4)) {
        *p = image::Rgb([px[2], px[1], px[0]]);
    }
    Some(img)
}

fn capture_printwindow(h: Hwnd) -> Option<RgbImage> {
    let (w, ht) = client_size(h);
    if w <= 0 || ht <= 0 {
        return None;
    }
    unsafe {
        let hdc = GetDC(Some(hwnd(h)));
        let mdc = CreateCompatibleDC(Some(hdc));
        let bmp = CreateCompatibleBitmap(hdc, w, ht);
        let old = SelectObject(mdc, bmp.into());
        // PW_CLIENTONLY | PW_RENDERFULLCONTENT
        let ok = PrintWindow(hwnd(h), mdc, PRINT_WINDOW_FLAGS(3)).as_bool();
        let img = if ok { read_bitmap(mdc, bmp, w, ht) } else { None };
        SelectObject(mdc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mdc);
        ReleaseDC(Some(hwnd(h)), hdc);
        img
    }
}

fn capture_screen(h: Hwnd) -> Option<RgbImage> {
    let (w, ht) = client_size(h);
    let (x, y) = client_origin(h);
    if w <= 0 || ht <= 0 {
        return None;
    }
    unsafe {
        let sdc = GetDC(None);
        let mdc = CreateCompatibleDC(Some(sdc));
        let bmp = CreateCompatibleBitmap(sdc, w, ht);
        let old = SelectObject(mdc, bmp.into());
        let ok = BitBlt(mdc, 0, 0, w, ht, Some(sdc), x, y, SRCCOPY | CAPTUREBLT).is_ok();
        let img = if ok { read_bitmap(mdc, bmp, w, ht) } else { None };
        SelectObject(mdc, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mdc);
        ReleaseDC(None, sdc);
        img
    }
}

fn looks_blank(img: &RgbImage) -> bool {
    let small = image::imageops::resize(&imgproc::to_gray(img), 32, 32, FilterType::CatmullRom);
    let lo = small.pixels().map(|p| p[0]).min().unwrap_or(0);
    let hi = small.pixels().map(|p| p[0]).max().unwrap_or(0);
    hi - lo < 6
}

/// ゲーム画面（クライアント領域）を撮影する．
pub fn capture_client(h: Hwnd, method: &str) -> Option<RgbImage> {
    if !window_exists(h) || is_minimized(h) {
        return None;
    }
    if method == "auto" || method == "printwindow" {
        if let Some(img) = capture_printwindow(h) {
            if !looks_blank(&img) {
                return Some(img);
            }
        }
        if method == "printwindow" {
            return None;
        }
    }
    capture_screen(h)
}

fn to_client_px(h: Hwnd, rx: f64, ry: f64) -> (i32, i32) {
    let (w, ht) = client_size(h);
    ((rx * w as f64).round_ties_even() as i32, (ry * ht as f64).round_ties_even() as i32)
}

fn click_postmessage(h: Hwnd, rx: f64, ry: f64) {
    let (x, y) = to_client_px(h, rx, ry);
    let lp = LPARAM((((y as u32) << 16) | (x as u32 & 0xFFFF)) as isize);
    unsafe {
        let _ = PostMessageW(Some(hwnd(h)), WM_MOUSEMOVE, WPARAM(0), lp);
        sleep(Duration::from_millis(30));
        let _ = PostMessageW(Some(hwnd(h)), WM_LBUTTONDOWN, WPARAM(1), lp); // MK_LBUTTON
        sleep(Duration::from_millis(60));
        let _ = PostMessageW(Some(hwnd(h)), WM_LBUTTONUP, WPARAM(0), lp);
    }
}

/// 実カーソルを動かしてクリックし，元の位置に戻す（ゲームを前面化する）．
fn click_sendinput(h: Hwnd, rx: f64, ry: f64) {
    let (x, y) = to_client_px(h, rx, ry);
    let (ox, oy) = client_origin(h);
    unsafe {
        let mut prev = POINT::default();
        let _ = GetCursorPos(&mut prev);
        let _ = SetForegroundWindow(hwnd(h));
        sleep(Duration::from_millis(50));
        let _ = SetCursorPos(ox + x, oy + y);
        sleep(Duration::from_millis(30));
        for flag in [MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP] {
            send_mouse(flag);
            sleep(Duration::from_millis(50));
        }
        let _ = SetCursorPos(prev.x, prev.y);
    }
}

unsafe fn send_mouse(flag: MOUSE_EVENT_FLAGS) {
    let inp = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 { mi: MOUSEINPUT { dx: 0, dy: 0, mouseData: 0, dwFlags: flag, time: 0, dwExtraInfo: 0 } },
    };
    SendInput(&[inp], std::mem::size_of::<INPUT>() as i32);
}

pub fn click(h: Hwnd, rx: f64, ry: f64, method: &str) {
    if method == "sendinput" {
        click_sendinput(h, rx, ry);
    } else {
        click_postmessage(h, rx, ry);
    }
}

/// 画像を CF_DIB（BITMAPINFOHEADER + 下から上の BGR）でクリップボードに入れる．
pub fn copy_image_to_clipboard(img: &RgbImage) -> bool {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let stride = (w * 3 + 3) & !3;
    let mut dib = Vec::with_capacity(40 + stride * h);
    let header = BITMAPINFOHEADER {
        biSize: 40,
        biWidth: w as i32,
        biHeight: h as i32,
        biPlanes: 1,
        biBitCount: 24,
        biCompression: BI_RGB.0,
        biSizeImage: (stride * h) as u32,
        ..Default::default()
    };
    dib.extend_from_slice(unsafe { std::slice::from_raw_parts(&header as *const _ as *const u8, 40) });
    for y in (0..h).rev() {
        let start = dib.len();
        for x in 0..w {
            let p = img.get_pixel(x as u32, y as u32);
            dib.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        dib.resize(start + stride, 0);
    }
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let ok = (|| -> windows::core::Result<()> {
            EmptyClipboard()?;
            let mem = GlobalAlloc(GMEM_MOVEABLE, dib.len())?;
            let ptr = GlobalLock(mem) as *mut u8;
            std::ptr::copy_nonoverlapping(dib.as_ptr(), ptr, dib.len());
            let _ = GlobalUnlock(mem);
            SetClipboardData(CF_DIB.0 as u32, Some(HANDLE(mem.0)))?;
            Ok(())
        })();
        let _ = CloseClipboard();
        if let Err(e) = &ok {
            eprintln!("[clipboard] コピー失敗: {e}");
        }
        ok.is_ok()
    }
}

/// フォルダ・ファイル・URL を既定のアプリで開く．
pub fn open_path(path: &str) {
    let _ = std::process::Command::new("explorer").arg(path).spawn();
}
