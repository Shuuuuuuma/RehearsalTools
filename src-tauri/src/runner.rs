//! 自動周回の本体（状態機械）と OCR ワーカー．
//!
//! 1 周の流れ：
//!   リハーサル画面待ち →「開始する」クリック → 対戦準備画面で「スキップ」が明るくなるのを待つ
//!   →「スキップ」クリック → 結果画面（「終了」ボタン）待ち → 撮影 → OCR キューへ →「終了」クリック
//! クリックは押せたかを画面の変化で確認し，変化がなければ再クリックする．

use std::collections::VecDeque;
use std::fs;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::Local;
use image::RgbImage;
use serde_json::{json, Value};

use crate::config::{f, point, regions, s, ConfigStore};
use crate::detector::{lineup_distance, visible, Checks, ScreenDetector};
use crate::ocr::{OcrEngine, ResultReader};
use crate::session::Session;
use crate::{paths, winapi};

enum Stop {
    Aborted,
    Timeout(&'static str),
    WindowLost,
    Error(String),
}

impl From<String> for Stop {
    fn from(e: String) -> Self {
        Stop::Error(e)
    }
}

impl From<&str> for Stop {
    fn from(e: &str) -> Self {
        Stop::Error(e.to_string())
    }
}

type Res<T> = Result<T, Stop>;

fn state_label(state: &str) -> &str {
    match state {
        "idle" => "待機中",
        "wait_page" => "リハーサル画面待ち",
        "click_start" => "「開始する」をクリック",
        "wait_skip" => "対戦準備中（スキップ待ち）",
        "click_skip" => "「スキップ」をクリック",
        "confirm" => "確認ダイアログ",
        "wait_result" => "結果画面待ち",
        "capture" => "結果を撮影",
        "click_end" => "「終了」をクリック",
        "finishing" => "集計中",
        other => other,
    }
}

fn now_sec() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn ms(v: &Value) -> Duration {
    Duration::from_secs_f64(f(v).max(0.0) / 1000.0)
}

/// 待ちの途中でも中断できるフラグ．
struct AbortFlag {
    flag: Mutex<bool>,
    cv: Condvar,
}

impl AbortFlag {
    fn set(&self, v: bool) {
        *self.flag.lock().unwrap() = v;
        self.cv.notify_all();
    }

    /// 指定時間待つ．中断されたら true．
    fn wait(&self, d: Duration) -> bool {
        let g = self.flag.lock().unwrap();
        let (g, _) = self.cv.wait_timeout_while(g, d, |a| !*a).unwrap();
        *g
    }
}

#[derive(Default)]
struct Status {
    running: bool,
    state: String,
    message: String,
    session: Option<String>,
    target: i64,
    completed: i64,
    current: i64,
    started_at: Option<f64>,
    error: Option<String>,
    last_result: Option<Value>,
    ocr_pending: i64,
    iter_times: Vec<f64>,
}

impl Status {
    fn blank() -> Self {
        Status { state: "idle".into(), ..Default::default() }
    }
}

pub type OnResult = Box<dyn Fn(&str, i64, &Value) + Send + Sync>;

pub struct AutomationRunner {
    config: Arc<ConfigStore>,
    pub detector: ScreenDetector,
    reader: Mutex<Option<Arc<ResultReader>>>,
    abort: AbortFlag,
    running: AtomicBool,
    status: Mutex<Status>,
    log: Mutex<VecDeque<String>>,
    pub last_iterations: AtomicI64,
    last_screen: Mutex<Option<RgbImage>>,
    on_result: OnResult,
}

impl AutomationRunner {
    pub fn new(config: Arc<ConfigStore>, on_result: OnResult) -> Arc<Self> {
        Arc::new(AutomationRunner {
            detector: ScreenDetector::new(config.clone()),
            config,
            reader: Mutex::new(None),
            abort: AbortFlag { flag: Mutex::new(false), cv: Condvar::new() },
            running: AtomicBool::new(false),
            status: Mutex::new(Status::blank()),
            log: Mutex::new(VecDeque::new()),
            last_iterations: AtomicI64::new(10),
            last_screen: Mutex::new(None),
            on_result,
        })
    }

    // ------------------------------------------------------------------ 公開 API
    pub fn reader(&self) -> Result<Arc<ResultReader>, String> {
        let mut r = self.reader.lock().unwrap();
        if r.is_none() {
            let ocfg = self.config.get()["ocr"].clone();
            let engine = OcrEngine::new(ocfg["engine"].as_str().unwrap_or("auto"), ocfg["lang"].as_str().unwrap_or("ja"))?;
            *r = Some(Arc::new(ResultReader::new(engine)));
        }
        Ok(r.clone().unwrap())
    }

    /// OCR 設定を変えた時に読み直させる．
    pub fn reset_reader(&self) {
        *self.reader.lock().unwrap() = None;
    }

    pub fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    pub fn start(self: &Arc<Self>, iterations: i64, session_id: Option<String>) -> Result<Value, String> {
        if iterations < 1 {
            return Err("周回数は 1 以上にしてください".into());
        }
        if self.running.swap(true, Ordering::SeqCst) {
            return Err("すでに実行中です".into());
        }
        if let Err(e) = self.reader() {
            // OCR が使えるか先に確認
            self.running.store(false, Ordering::SeqCst);
            return Err(e);
        }
        self.last_iterations.store(iterations, Ordering::SeqCst);
        self.abort.set(false);
        {
            let mut st = self.status.lock().unwrap();
            *st = Status::blank();
            st.running = true;
            st.target = iterations;
            st.started_at = Some(now_sec());
        }
        let me = self.clone();
        thread::spawn(move || me.run(iterations, session_id));
        Ok(self.get_status())
    }

    pub fn abort(&self) {
        if self.running() {
            self.abort.set(true);
            self.logline("中断を要求しました");
        }
    }

    pub fn get_status(&self) -> Value {
        let st = self.status.lock().unwrap();
        let avg = (!st.iter_times.is_empty()).then(|| (st.iter_times.iter().sum::<f64>() / st.iter_times.len() as f64 * 10.0).round() / 10.0);
        let remaining = (st.target - st.completed).max(0);
        let eta = avg.filter(|_| st.running).map(|a| (remaining as f64 * a).round() as i64);
        let elapsed = st.started_at.map(|t| (now_sec() - t).round() as i64);
        let log: Vec<String> = {
            let l = self.log.lock().unwrap();
            l.iter().skip(l.len().saturating_sub(80)).cloned().collect()
        };
        json!({
            "running": st.running,
            "state": st.state,
            "message": st.message,
            "session": st.session,
            "target": st.target,
            "completed": st.completed,
            "current": st.current,
            "started_at": st.started_at,
            "error": st.error,
            "last_result": st.last_result,
            "ocr_pending": st.ocr_pending,
            "state_label": state_label(&st.state),
            "avg_iteration_sec": avg,
            "eta_sec": eta,
            "elapsed_sec": elapsed,
            "log": log,
        })
    }

    /// ゲーム画面を撮って保存し，クリップボードにもコピーする（Ctrl+Shift+S）．
    pub fn screenshot(&self) -> Result<Value, String> {
        let cfg = self.config.get();
        let h = winapi::find_window(s(&cfg["window_title"])).ok_or("ゲームウィンドウが見つかりません")?;
        let img = winapi::capture_client(h, s(&cfg["capture_method"])).ok_or("キャプチャに失敗しました")?;
        let dir = paths::screenshot_dir();
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let path = dir.join(Local::now().format("%Y%m%d_%H%M%S_%3f.png").to_string());
        img.save(&path).map_err(|e| e.to_string())?;
        let copied = winapi::copy_image_to_clipboard(&img);
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        self.logline(&format!("スクリーンショット保存: {name}{}", if copied { "（クリップボードにもコピー）" } else { "" }));
        Ok(json!({"path": path.to_string_lossy(), "clipboard": copied}))
    }

    // ------------------------------------------------------------------ 内部
    fn set(&self, f: impl FnOnce(&mut Status)) {
        f(&mut self.status.lock().unwrap());
    }

    fn set_state(&self, state: &str) {
        self.set(|s| s.state = state.to_string());
    }

    pub fn logline(&self, msg: &str) {
        let line = format!("{} {msg}", Local::now().format("%H:%M:%S"));
        println!("[auto] {line}");
        let mut l = self.log.lock().unwrap();
        l.push_back(line);
        while l.len() > 300 {
            l.pop_front();
        }
    }

    fn sleep(&self, d: Duration) -> Res<()> {
        if self.abort.wait(d) {
            Err(Stop::Aborted)
        } else {
            Ok(())
        }
    }

    fn grab(&self, h: winapi::Hwnd) -> Res<RgbImage> {
        let method = s(&self.config.get()["capture_method"]).to_string();
        for _ in 0..10 {
            if !winapi::window_exists(h) {
                return Err(Stop::WindowLost);
            }
            if let Some(img) = winapi::capture_client(h, &method) {
                *self.last_screen.lock().unwrap() = Some(img.clone());
                return Ok(img);
            }
            self.sleep(Duration::from_millis(300))?;
        }
        Err(Stop::Error("キャプチャに失敗し続けています（ウィンドウが最小化されていませんか？）".into()))
    }

    fn click(&self, h: winapi::Hwnd, button: &str) {
        let cfg = self.config.get();
        let (x, y) = point(&cfg["buttons"][button]["click"]);
        winapi::click(h, x, y, s(&cfg["click_method"]));
    }

    fn wait(
        &self,
        h: winapi::Hwnd,
        cond: impl Fn(&Checks) -> bool,
        timeout_key: &'static str,
        confirm: i64,
        mut on_poll: impl FnMut(&Checks) -> Res<()>,
    ) -> Res<RgbImage> {
        let cfg = self.config.get();
        let interval = ms(&cfg["poll_interval_ms"]);
        let deadline = Instant::now() + ms(&cfg[timeout_key]);
        let mut hits = 0;
        loop {
            let screen = self.grab(h)?;
            let checks = self.detector.check_all(&screen);
            if cond(&checks) {
                hits += 1;
                if hits >= confirm {
                    return Ok(screen);
                }
            } else {
                hits = 0;
                on_poll(&checks)?;
            }
            if Instant::now() > deadline {
                return Err(Stop::Timeout(timeout_key));
            }
            self.sleep(interval)?;
        }
    }

    /// 対戦準備画面で「コンテスト準備中...」が消えてスキップが押せる状態になるまで待つ．
    ///
    /// 準備中の表示が見えている間はタイムアウトしない（PC が遅くても止まらない）．
    /// loading_timeout_ms は「準備中もスキップも見えない」状態が続いた場合だけに効く．
    fn wait_prepared(&self, h: winapi::Hwnd) -> Res<Checks> {
        let cfg = self.config.get();
        let interval = ms(&cfg["poll_interval_ms"]);
        let idle_limit = ms(&cfg["loading_timeout_ms"]);
        let prep_limit = ms(&cfg["prepare_timeout_ms"]);
        let retry_iv = ms(&cfg["click_retry_interval_ms"]);
        let mut last_seen = Instant::now(); // 最後に「準備中」か「スキップ」を見た時刻
        let mut prep_started: Option<Instant> = None;
        let mut hits = 0;
        let mut last_confirm: Option<Instant> = None;
        let base_msg = |s: &Status| s.message.split('｜').next().unwrap_or("").to_string();
        loop {
            let checks = self.detector.check_all(&self.grab(h)?);
            let now = Instant::now();
            if visible(&checks, "confirm") {
                // 空き枠がある編成：「メモリー未編成」の確認ダイアログ →「開始する」
                hits = 0;
                last_seen = now;
                if last_confirm.is_none_or(|t| now - t >= retry_iv) {
                    if last_confirm.is_none() {
                        self.logline("「メモリー未編成」の確認 →「開始する」をクリック");
                    }
                    self.click(h, "confirm");
                    last_confirm = Some(now);
                }
            } else if visible(&checks, "preparing") {
                hits = 0;
                last_seen = now;
                if prep_started.is_none() {
                    prep_started = Some(now);
                    self.set(|s| s.message = base_msg(s) + "｜コンテスト準備中…");
                }
                if !prep_limit.is_zero() && now - prep_started.unwrap() > prep_limit {
                    return Err(Stop::Timeout("prepare_timeout_ms"));
                }
            } else if visible(&checks, "skip") || (visible(&checks, "end") && !visible(&checks, "start")) {
                hits += 1;
                last_seen = now;
                if hits >= 2 {
                    if let Some(t) = prep_started {
                        self.logline(&format!("準備完了（{:.1} 秒）", (now - t).as_secs_f64()));
                    }
                    self.set(|s| s.message = base_msg(s));
                    return Ok(checks);
                }
            } else {
                hits = 0;
                if now - last_seen > idle_limit {
                    return Err(Stop::Timeout("loading_timeout_ms"));
                }
            }
            self.sleep(interval)?;
        }
    }

    /// ボタンを押し，画面が変わる（gone が真）まで間隔をあけて押し直す．
    fn click_until(&self, h: winapi::Hwnd, button: &str, gone: impl Fn(&Checks) -> bool) -> Res<()> {
        let cfg = self.config.get();
        let retries = cfg["max_click_retries"].as_i64().unwrap_or(5);
        let retry_iv = ms(&cfg["click_retry_interval_ms"]);
        let interval = ms(&cfg["poll_interval_ms"]);
        self.click(h, button);
        let (mut clicks, mut last) = (1, Instant::now());
        let mut ok = 0;
        loop {
            self.sleep(interval)?;
            let checks = self.detector.check_all(&self.grab(h)?);
            if gone(&checks) {
                ok += 1;
                if ok >= 2 {
                    return Ok(());
                }
                continue;
            }
            ok = 0;
            if last.elapsed() >= retry_iv {
                if clicks > retries {
                    return Err(Stop::Error(format!(
                        "「{button}」を {clicks} 回押しても画面が変わりません（クリック方式の設定を確認してください）"
                    )));
                }
                self.logline(&format!("「{button}」再クリック（{} 回目）", clicks + 1));
                self.click(h, button);
                clicks += 1;
                last = Instant::now();
            }
        }
    }

    fn run(self: Arc<Self>, iterations: i64, session_id: Option<String>) {
        let cfg = self.config.get();
        let mut session: Option<Arc<Session>> = None;
        let mut worker: Option<(mpsc::Sender<Option<(i64, std::path::PathBuf)>>, mpsc::Receiver<()>)> = None;
        let mut done = 0;

        let result: Res<()> = (|| {
            let title = s(&cfg["window_title"]);
            let h = winapi::find_window(title).ok_or_else(|| format!("ゲームウィンドウ「{title}」が見つかりません"))?;
            let sess = Arc::new(match &session_id {
                Some(id) => Session::open(id).ok_or("セッションが見つかりません")?,
                None => Session::create("").map_err(|e| e.to_string())?,
            });
            session = Some(sess.clone());
            self.set(|s| s.session = Some(sess.id.clone()));
            self.logline(&format!("開始: {iterations} 周（セッション {}{}）", sess.id, if session_id.is_some() { "・追加" } else { "" }));
            let (tx, rx) = mpsc::channel();
            let (done_tx, done_rx) = mpsc::channel();
            {
                let me = self.clone();
                let sess = sess.clone();
                thread::spawn(move || {
                    me.ocr_worker(&sess, rx);
                    let _ = done_tx.send(());
                });
            }
            worker = Some((tx.clone(), done_rx));

            for n in 0..iterations {
                let it = sess.next_iteration();
                self.set(|s| {
                    s.current = n + 1;
                    s.message = format!("{}/{iterations} 周目（通し番号 {it}）", n + 1);
                });
                let t0 = Instant::now();
                self.one_iteration(h, &sess, it, n == 0, &tx)?;
                done += 1;
                self.set(|s| {
                    s.completed = done;
                    s.iter_times.push(t0.elapsed().as_secs_f64());
                });
            }
            self.logline(&format!("完了: {done} 周"));
            Ok(())
        })();

        match result {
            Ok(()) => {}
            Err(Stop::Aborted) => {
                self.logline(&format!("中断しました（{done} 周完了）"));
                self.set(|s| {
                    s.error = None;
                    s.message = format!("中断（{done} 周完了）");
                });
            }
            Err(Stop::Timeout(key)) => {
                let sec = cfg[key].as_i64().unwrap_or(0) / 1000;
                let msg = match key {
                    "page_timeout_ms" => format!("リハーサル画面が {sec} 秒以内に検出できず"),
                    "loading_timeout_ms" => format!("開始後 {sec} 秒間，「準備中」表示もスキップボタンも検出できず"),
                    "prepare_timeout_ms" => format!("「コンテスト準備中」が {sec} 秒以上続いたため"),
                    "result_timeout_ms" => format!("結果画面が {sec} 秒以内に検出できず"),
                    _ => format!("{key} {sec}"),
                };
                let mut text = format!("{msg}停止しました（設定の {key} / 参照画像を確認）");
                // 止まった瞬間の画面を残す（原因調査用）
                if let (Some(sess), Some(last)) = (&session, self.last_screen.lock().unwrap().clone()) {
                    let p = sess.dir.join(format!("stopped_{}.png", Local::now().format("%H%M%S")));
                    if last.save(&p).is_ok() {
                        let checks = self.detector.check_all(&last);
                        text += &format!("　停止時の画面: {}", p.file_name().unwrap().to_string_lossy());
                        let marks: Vec<String> =
                            checks.iter().map(|(k, v)| format!("{k}={}", if v["visible"] == json!(true) { "○" } else { "×" })).collect();
                        self.logline(&format!("停止時の判定: {}", marks.join(", ")));
                    }
                }
                self.logline(&text);
                self.set(|s| s.error = Some(text));
            }
            Err(Stop::WindowLost) => {
                self.logline("ゲームウィンドウが閉じられたため停止しました");
                self.set(|s| s.error = Some("ゲームウィンドウが閉じられました".into()));
            }
            Err(Stop::Error(e)) => {
                self.logline(&format!("エラーで停止: {e}"));
                self.set(|s| s.error = Some(e));
            }
        }

        self.set_state("finishing");
        if let Some((tx, done_rx)) = worker {
            let _ = tx.send(None);
            let _ = done_rx.recv_timeout(Duration::from_secs(120));
        }
        if let Some(sess) = session {
            sess.write_statistics();
            self.logline("統計を出力しました");
        }
        self.set(|s| {
            s.running = false;
            s.state = "idle".into();
        });
        self.running.store(false, Ordering::SeqCst);
    }

    fn one_iteration(
        &self,
        h: winapi::Hwnd,
        session: &Session,
        it: i64,
        first: bool,
        ocr_q: &mpsc::Sender<Option<(i64, std::path::PathBuf)>>,
    ) -> Res<()> {
        let cfg = self.config.get();
        let confirm = cfg["confirm_count"].as_i64().unwrap_or(3);

        // 1) リハーサル画面待ち（結果画面のまま止まっていたら「終了」を押す）
        self.set_state("wait_page");
        let screen = self.wait(
            h,
            |c| visible(c, "start"),
            "page_timeout_ms",
            confirm,
            |c| {
                if visible(c, "end") && !visible(c, "skip") {
                    self.logline("結果画面が残っていたため「終了」をクリック");
                    self.click(h, "end");
                    self.sleep(Duration::from_secs(1))?;
                }
                Ok(())
            },
        )?;

        // 2) 初回だけ開始画面を保存／追加周回なら編成が同じか確認
        if first {
            if let Some(prev) = session.load_start_page() {
                let thr = f(&cfg["lineup_threshold"]);
                let dist = lineup_distance(&screen, &prev, &regions(&cfg["regions"]["icons"]));
                let worst = dist.iter().cloned().fold(0.0, f64::max);
                self.logline(&format!("編成チェック: 最大差 {worst:.1}（しきい値 {thr}）"));
                if worst > thr {
                    let bad: Vec<String> =
                        dist.iter().enumerate().filter(|(_, &d)| d > thr).map(|(i, _)| format!("S{}C{}", i / 3 + 1, i % 3 + 1)).collect();
                    return Err(Stop::Error(format!(
                        "編成がセッション開始時と違います（{}）．新しいセッションで周回して，あとで「セッション」タブの結合でまとめてください",
                        bad.join(", ")
                    )));
                }
            } else {
                screen.save(&session.start_page).map_err(|e| e.to_string())?;
            }
        }

        // 3) 開始
        self.set_state("click_start");
        self.click_until(h, "start", |c| !visible(c, "start"))?;

        // 4) 対戦準備 → スキップ
        self.set_state("wait_skip");
        let checks = self.wait_prepared(h)?;
        if visible(&checks, "skip") {
            self.set_state("click_skip");
            self.click_until(h, "skip", |c| !visible(c, "skip"))?;
        }

        // 5) 結果画面
        self.set_state("wait_result");
        self.wait(
            h,
            |c| visible(c, "end") && !visible(c, "skip") && !visible(c, "start"),
            "result_timeout_ms",
            confirm,
            |_| Ok(()),
        )?;
        self.set_state("capture");
        self.sleep(ms(&cfg["capture_delay_ms"]))?;
        let shot = self.grab(h)?;
        let path = session.screenshot_path(it);
        shot.save(&path).map_err(|e| e.to_string())?;
        let _ = ocr_q.send(Some((it, path)));
        self.set(|s| s.ocr_pending += 1);

        // 6) 終了
        self.set_state("click_end");
        self.click_until(h, "end", |c| !visible(c, "end"))
    }

    fn ocr_worker(&self, session: &Session, rx: mpsc::Receiver<Option<(i64, std::path::PathBuf)>>) {
        let reader = self.reader();
        while let Ok(Some((it, path))) = rx.recv() {
            let res = reader.clone().and_then(|r| {
                let img = image::open(&path).map_err(|e| e.to_string())?.to_rgb8();
                Ok(r.read(&img, &self.config.get()))
            });
            match res {
                Ok(res) => {
                    session.append_result(it, &res);
                    let totals: Vec<String> = res["stages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|st| st["total"].as_i64().filter(|&t| t != 0).map(fmt_int).unwrap_or_else(|| "?".into()))
                        .collect();
                    let flags = s(&res["flags"]);
                    self.logline(&format!(
                        "#{it} OCR {}  合計 {}{}",
                        s(&res["status"]),
                        totals.join(", "),
                        if flags.is_empty() { String::new() } else { format!("  [{flags}]") }
                    ));
                    let stages: Vec<Value> = res["stages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|st| json!({"scores": st["scores"], "total": st["total"], "bonus": st["bonus"], "status": st["status"]}))
                        .collect();
                    self.set(|s| s.last_result = Some(json!({"iteration": it, "status": res["status"], "flags": res["flags"], "stages": stages})));
                    (self.on_result)(&session.id, it, &res);
                }
                Err(e) => {
                    self.logline(&format!("#{it} OCR 失敗: {e}"));
                    let blank = json!({"scores": [null, null, null], "total": null, "bonus": null});
                    session.append_result(it, &json!({"stages": [blank, blank, blank], "status": "flagged", "flags": format!("error:{e}")}));
                }
            }
            self.set(|s| s.ocr_pending = (s.ocr_pending - 1).max(0));
        }
    }
}

/// 1234567 → "1,234,567"
pub fn fmt_int(v: i64) -> String {
    let s = v.abs().to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if v < 0 {
        format!("-{out}")
    } else {
        out
    }
}
