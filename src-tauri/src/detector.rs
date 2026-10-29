//! 画面状態の判定：ボタン領域を参照画像と比較して「今どの画面か」を判断する．

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use image::{imageops, imageops::FilterType, RgbImage};
use serde_json::{json, Map, Value};

use crate::config::{f, region, s, ConfigStore};
use crate::imgproc::{
    brightness, crop_rel, hist_correl, hs_hist, match_template, max_loc, py_round, resize_rgb, to_gray, white_ratio,
    FloatImg,
};

pub type Checks = Map<String, Value>;

pub fn visible(c: &Checks, name: &str) -> bool {
    c.get(name).and_then(|v| v["visible"].as_bool()).unwrap_or(false)
}

fn round_to(v: f64, d: i32) -> f64 {
    let p = 10f64.powi(d);
    (v * p).round_ties_even() / p
}

/// 0〜1 の類似度．グレースケールの正規化相関と色ヒストグラム相関の平均．
pub fn similarity(a: &RgbImage, refimg: &RgbImage) -> f64 {
    let a = resize_rgb(a, refimg.width(), refimg.height(), FilterType::Triangle);
    let (ga, gr) = (FloatImg::cv_gray(&a), FloatImg::cv_gray(refimg));
    let ncc = if ga.std() < 1e-3 || gr.std() < 1e-3 {
        1.0 - ((ga.mean() - gr.mean()).abs() / 64.0).min(1.0)
    } else {
        match_template(&ga, &gr).d.first().copied().unwrap_or(0.0) as f64
    };
    let hist = hist_correl(&hs_hist(&a), &hs_hist(refimg));
    (ncc.max(0.0) + hist.max(0.0)) / 2.0
}

/// 判定領域を少し広げて参照画像の位置を探し，一番合う場所での類似度を返す．
/// ウィンドウの大きさや画面の描画が数ピクセルずれても判定が崩れないようにするため．
pub fn shifted_similarity(screen: &RgbImage, reg: [f64; 4], refimg: &RgbImage) -> f64 {
    let (mx, my) = (0.02, 0.008);
    let [x, y, w, h] = reg;
    let big = crop_rel(screen, [x - mx, y - my, w + 2.0 * mx, h + 2.0 * my]);
    let base = crop_rel(screen, reg);
    // 判定領域を参照画像の大きさに合わせる倍率で，広げた領域も縮尺する
    let sx = refimg.width() as f64 / base.width().max(1) as f64;
    let sy = refimg.height() as f64 / base.height().max(1) as f64;
    let bw = (refimg.width() as i64).max(py_round(big.width() as f64 * sx)) as u32;
    let bh = (refimg.height() as i64).max(py_round(big.height() as f64 * sy)) as u32;
    let bigr = resize_rgb(&big, bw, bh, FilterType::Triangle);
    let gr = FloatImg::cv_gray(refimg);
    let best = similarity(&base, refimg);
    if gr.std() < 1e-3 {
        return best;
    }
    let res = match_template(&FloatImg::cv_gray(&bigr), &gr);
    if res.d.is_empty() {
        return best;
    }
    let (bx, by) = max_loc(&res);
    let patch = imageops::crop_imm(&bigr, bx as u32, by as u32, refimg.width(), refimg.height()).to_image();
    best.max(similarity(&patch, refimg))
}

/// 「開始する」「終了」のようなオレンジボタンの画素割合（参照画像が無い時の代替判定）．
fn orange_ratio(img: &RgbImage) -> f64 {
    let n = (img.width() * img.height()).max(1) as f64;
    img.pixels()
        .filter(|p| {
            let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
            r > 220 && g > 70 && g < 200 && b < 110 && r - b > 120
        })
        .count() as f64
        / n
}

/// 編成（9 枠アイコン）の比較．16×16 のグレースケールの平均差．
pub fn icon_small(img: &RgbImage, reg: [f64; 4]) -> Vec<f32> {
    let g = to_gray(&crop_rel(img, reg));
    imageops::resize(&g, 16, 16, FilterType::CatmullRom).pixels().map(|p| p[0] as f32).collect()
}

pub fn icon_distance(a: &[f32], b: &[f32]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y).abs() as f64).sum::<f64>() / a.len().max(1) as f64
}

pub fn lineup_distance(a: &RgbImage, b: &RgbImage, icons: &[[f64; 4]]) -> Vec<f64> {
    icons.iter().map(|&r| icon_distance(&icon_small(a, r), &icon_small(b, r))).collect()
}

pub struct ScreenDetector {
    config: Arc<ConfigStore>,
    refs: Mutex<HashMap<String, (SystemTime, Arc<RgbImage>)>>,
}

pub const BUTTONS: [&str; 4] = ["start", "skip", "end", "confirm"];

impl ScreenDetector {
    pub fn new(config: Arc<ConfigStore>) -> Self {
        ScreenDetector { config, refs: Mutex::new(HashMap::new()) }
    }

    fn ref_path(cfg: &Value, name: &str) -> PathBuf {
        ConfigStore::reference_path(s(&cfg["buttons"][name]["reference"]))
    }

    fn reference(&self, cfg: &Value, name: &str) -> Option<Arc<RgbImage>> {
        let path = Self::ref_path(cfg, name);
        let mt = std::fs::metadata(&path).and_then(|m| m.modified()).ok()?;
        let mut refs = self.refs.lock().unwrap();
        if let Some((t, img)) = refs.get(name) {
            if *t == mt {
                return Some(img.clone());
            }
        }
        let img = Arc::new(image::open(&path).ok()?.to_rgb8());
        refs.insert(name.to_string(), (mt, img.clone()));
        Some(img)
    }

    pub fn has_reference(&self, name: &str) -> bool {
        self.reference(&self.config.get(), name).is_some()
    }

    pub fn save_reference(&self, name: &str, screen: &RgbImage) -> Result<PathBuf, String> {
        let cfg = self.config.get();
        let path = Self::ref_path(&cfg, name);
        crop_rel(screen, region(&cfg["buttons"][name]["region"])).save(&path).map_err(|e| e.to_string())?;
        self.refs.lock().unwrap().remove(name);
        Ok(path)
    }

    /// ボタンが見えているかを判定し，判定に使った数値も返す．
    fn check(&self, cfg: &Value, name: &str, screen: &RgbImage) -> Value {
        let bcfg = &cfg["buttons"][name];
        let reg = region(&bcfg["region"]);
        let crop = crop_rel(screen, reg);
        let bright = round_to(brightness(&crop), 1);
        let mut info = json!({"button": name, "brightness": bright});
        let thr = f(&cfg["match_threshold"]);

        let mut vis = if let Some(r) = self.reference(cfg, name) {
            let sim = shifted_similarity(screen, reg, &r);
            info["similarity"] = json!(round_to(sim, 3));
            sim >= thr
        } else if name != "skip" {
            let ratio = orange_ratio(&crop);
            info["orange_ratio"] = json!(round_to(ratio, 3));
            info["note"] = json!("参照画像なし（色で代替判定）");
            ratio >= 0.45
        } else {
            // スキップ：暗いボタン地に白文字・白枠．白が少しだけ・暗部が大半，という特徴で代替判定
            let white = white_ratio(&crop, 200);
            let n = (crop.width() * crop.height()).max(1) as f64;
            let dark = crop.pixels().filter(|p| p[0].max(p[1]).max(p[2]) < 90).count() as f64 / n;
            info["white_ratio"] = json!(round_to(white, 3));
            info["dark_ratio"] = json!(round_to(dark, 3));
            info["note"] = json!("参照画像なし（形で代替判定）");
            (0.04..=0.3).contains(&white) && dark >= 0.5
        };

        if name == "skip" {
            vis = vis && bright >= bcfg["brightness_min"].as_f64().unwrap_or(0.0);
        }
        if name == "confirm" {
            // 確認ダイアログは左に白い「キャンセル」がある．これが無ければダイアログではない
            let cw = white_ratio(&crop_rel(screen, region(&bcfg["cancel_region"])), 225);
            info["cancel_white"] = json!(round_to(cw, 3));
            vis = vis && cw >= 0.5;
        }
        info["visible"] = json!(vis);
        info
    }

    /// 右下の「コンテスト準備中...」が出ているか（白文字の画素割合で判定）．
    fn check_preparing(cfg: &Value, screen: &RgbImage) -> Value {
        let p = &cfg["preparing"];
        let white = white_ratio(&crop_rel(screen, region(&p["region"])), 215);
        json!({"button": "preparing", "white_ratio": round_to(white, 3), "visible": white >= f(&p["white_min"])})
    }

    pub fn check_all(&self, screen: &RgbImage) -> Checks {
        let cfg = self.config.get();
        let mut out = Checks::new();
        for n in BUTTONS {
            out.insert(n.to_string(), self.check(&cfg, n, screen));
        }
        let prep = Self::check_preparing(&cfg, screen);
        if prep["visible"] == json!(true) {
            out["skip"]["visible"] = json!(false); // 準備中はスキップを押せる状態とみなさない
        }
        out.insert("preparing".into(), prep);
        if visible(&out, "confirm") {
            // ダイアログの下のボタンは押せないので見えていない扱い
            for k in ["start", "skip", "end"] {
                out[k]["visible"] = json!(false);
            }
        }
        out
    }
}
