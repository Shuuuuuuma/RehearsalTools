//! 学マス リハーサル自動周回＆スコア集計ツール（Tauri 版）．
//!
//! 画面は static/*.html をそのまま使い，app:// の独自プロトコルでページと /api/... を返す．
//! （同じオリジンなので，画面の fetch("/api/...") や <img src="/api/..."> は Python 版のまま動く）

// 本番の exe ではコンソール窓を出さない
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod api;
#[cfg(test)]
mod checks;
mod config;
mod dashboard;
mod detector;
mod hotkeys;
mod imgproc;
mod ocr;
mod paths;
mod runner;
mod session;
mod storage;
mod winapi;
mod winocr;

use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use tauri::http::{header::CONTENT_TYPE, Request, Response};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use crate::storage::StageScores;

/// 画面の HTML．開発時は編集がすぐ反映されるようファイルから読む．
fn page(name: &str) -> Option<String> {
    #[cfg(debug_assertions)]
    {
        std::fs::read_to_string(paths::data_root().join("static").join(name)).ok()
    }
    #[cfg(not(debug_assertions))]
    {
        match name {
            "index.html" => Some(include_str!("../../static/index.html").to_string()),
            "automation.html" => Some(include_str!("../../static/automation.html").to_string()),
            _ => None,
        }
    }
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                Some(v) => {
                    out.push(v);
                    i += 2;
                }
                None => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|kv| !kv.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn serve(app: &api::App, req: &Request<Vec<u8>>) -> Response<Vec<u8>> {
    let path = req.uri().path().to_string();
    let r = if path.starts_with("/api/") {
        let query = parse_query(req.uri().query().unwrap_or(""));
        app.handle(req.method().as_str(), &path, &query, req.body())
    } else {
        let name = match path.as_str() {
            "/" | "/index.html" => "index.html",
            "/automation" | "/automation.html" => "automation.html",
            _ => "",
        };
        page(name)
            .map(|html| api::Resp { status: 200, content_type: "text/html; charset=utf-8", body: html.into_bytes() })
            .ok_or((404, "見つかりません".to_string()))
    };
    let r = r.unwrap_or_else(|(code, msg)| api::error_response(code, &msg));
    Response::builder()
        .status(r.status)
        .header(CONTENT_TYPE, r.content_type)
        .header("Cache-Control", "no-store")
        .body(r.body)
        .unwrap()
}

fn main() {
    let config = Arc::new(config::ConfigStore::new());
    let storage = Arc::new(storage::ScoreStorage::open().expect("スコア DB を開けません"));

    // 自動周回の OCR 結果を集計ダッシュボード用 DB にも登録（要確認の結果は除く）
    let on_result: runner::OnResult = {
        let storage = storage.clone();
        Box::new(move |sid, it, res| {
            if res["status"] == "flagged" {
                return;
            }
            let stages: Vec<StageScores> = (0..3)
                .map(|i| StageScores {
                    stage: i as i64 + 1,
                    scores: res["stages"][i]["scores"].as_array().map(|a| a.iter().map(|v| v.as_i64()).collect()).unwrap_or_default(),
                })
                .collect();
            if let Err(e) = storage.add_trial_scores(&format!("{sid}-{it:04}"), &stages) {
                eprintln!("[auto] DB 登録エラー: {e}");
            }
        })
    };
    let runner = runner::AutomationRunner::new(config.clone(), on_result);
    let scanner = {
        let config = config.clone();
        dashboard::AutoResultScanner::new(storage.clone(), Box::new(move || config::s(&config.get()["capture_method"]).to_string()))
    };

    // ホットキー：Ctrl+Shift+A 開始 / Ctrl+Shift+Q 中断 / Ctrl+Shift+S スクショ
    let registered = Arc::new(Mutex::new(Vec::new()));
    {
        let (r1, r2, r3) = (runner.clone(), runner.clone(), runner.clone());
        let actions: Vec<(char, hotkeys::Action)> = vec![
            ('A', Arc::new(move || {
                if !r1.running() {
                    if let Err(e) = r1.start(r1.last_iterations.load(Ordering::SeqCst), None) {
                        r1.logline(&format!("開始できません: {e}"));
                    }
                }
            })),
            ('Q', Arc::new(move || r2.abort())),
            ('S', Arc::new(move || {
                if let Err(e) = r3.screenshot() {
                    r3.logline(&format!("スクリーンショット失敗: {e}"));
                }
            })),
        ];
        hotkeys::start(actions, registered.clone());
    }

    let app = Arc::new(api::App::new(config, runner, storage, scanner, registered));
    let proto_app = app.clone();

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            // 二重に起動された時は，今のウィンドウを前に出すだけ
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.set_focus();
            }
        }))
        .register_asynchronous_uri_scheme_protocol("app", move |_ctx, request, responder| {
            let app = proto_app.clone();
            // OCR や読み直しは時間がかかるので，画面を止めないよう別スレッドで処理する
            std::thread::spawn(move || {
                let resp = catch_unwind(AssertUnwindSafe(|| serve(&app, &request)))
                    .unwrap_or_else(|_| {
                        let r = api::error_response(500, "内部エラーが発生しました");
                        Response::builder().status(500).header(CONTENT_TYPE, r.content_type).body(r.body).unwrap()
                    });
                responder.respond(resp);
            });
        })
        .setup(move |handle| {
            let url = WebviewUrl::CustomProtocol("app://localhost/automation".parse().unwrap());
            let w = WebviewWindowBuilder::new(handle, "main", url)
                .title("学マス リハーサル自動周回＆スコア集計ツール")
                .inner_size(1440.0, 920.0)
                // CSV のドラッグ＆ドロップを画面側（HTML）で受け取る
                .disable_drag_drop_handler()
                .build()?;
            let _ = app.window.set(w);
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("起動に失敗しました");
}
