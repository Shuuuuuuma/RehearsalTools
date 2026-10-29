//! グローバルホットキー（ツールのウィンドウが前面でなくても効く）．
//!
//!   Ctrl+Shift+A : 自動周回を開始（前回の周回数）
//!   Ctrl+Shift+Q : 自動周回を中断
//!   Ctrl+Shift+S : ゲーム画面のスクリーンショット（保存＋クリップボード）

use std::sync::{Arc, Mutex};
use std::thread;

use windows::Win32::UI::Input::KeyboardAndMouse::{RegisterHotKey, MOD_CONTROL, MOD_NOREPEAT, MOD_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

pub type Action = Arc<dyn Fn() + Send + Sync>;

/// 登録できたホットキーの表示名を registered に入れ，メッセージループを回すスレッドを起動する．
pub fn start(actions: Vec<(char, Action)>, registered: Arc<Mutex<Vec<String>>>) {
    thread::spawn(move || unsafe {
        let mut ids = Vec::new();
        for (i, (key, action)) in actions.iter().enumerate() {
            let id = i as i32 + 1;
            let vk = key.to_ascii_uppercase() as u32;
            if RegisterHotKey(None, id, MOD_CONTROL | MOD_SHIFT | MOD_NOREPEAT, vk).is_ok() {
                ids.push((id, action.clone()));
                registered.lock().unwrap().push(format!("Ctrl+Shift+{}", key.to_ascii_uppercase()));
            } else {
                eprintln!("[hotkey] Ctrl+Shift+{} は他のアプリが使用中のため登録できませんでした", key.to_ascii_uppercase());
            }
        }
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.message == WM_HOTKEY {
                if let Some((_, action)) = ids.iter().find(|(id, _)| *id as usize == msg.wParam.0) {
                    let action = action.clone();
                    thread::spawn(move || action());
                }
            }
        }
    });
}
