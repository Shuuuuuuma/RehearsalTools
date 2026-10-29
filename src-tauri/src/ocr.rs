//! リハーサル結果画面の数字読み取りと検算．
//!
//! 学マスの結果画面は数字のフォントが決まっているので，汎用 OCR ではなく
//! 「数字 1 文字ずつ切り出して，見本（テンプレート）と比べる」方式で読む．
//!
//! 各ステージについて
//!   - 3 人のスコア（白文字）
//!   - ステージ合計（白文字・大）
//!   - 1 位ボーナス（1 位のアイドルの上に出る白い吹き出しの青文字 "+12345"．位置は 1 位の列によって変わる）
//! を読み，「3 人のスコア + ボーナス = ステージ合計」で検算する．
//! 合わない場合は，しきい値を変えた読み直しと「2 番目に似ている数字」への置き換えを試し，
//! 式が成り立つ最も自然な組み合わせを採用する（= repaired）．それでもだめなら flagged．

use std::collections::HashMap;
use std::fs;
use std::io::{Cursor, Read, Write};
use std::sync::Mutex;

use image::{imageops, imageops::FilterType, Luma, RgbImage};
use serde_json::{json, Value};

use crate::config::{f, region, s};
use crate::imgproc::{connected_components, crop_rel, morph_close, py_round, resize_area_f32, Mask};
use crate::{paths, winocr};

const SCORE_MIN: i64 = 1; // キャラが少ない・弱いと 3 桁のスコアもある
const SCORE_MAX: i64 = 9_999_999;
const TOTAL_MIN: i64 = 1;
const TOTAL_MAX: i64 = 29_999_999;
const GW: usize = 12; // 正規化した 1 文字の大きさ
const GH: usize = 16;
const FEAT: usize = GW * GH;

static BUILTIN_TEMPLATES: &[u8] = include_bytes!("../resources/digit_templates.npz");

fn user_templates_path() -> std::path::PathBuf {
    paths::data_dir().join("digit_templates_user.npz")
}

// --------------------------------------------------------------------------
// 二値化
// --------------------------------------------------------------------------
/// 白い文字 → 1．
fn white_text_mask(img: &RgbImage, threshold: i64) -> Mask {
    Mask::from_fn(img, |p| p[0].min(p[1]).min(p[2]) as i64 >= threshold)
}

/// 白い吹き出しの中の青文字 → 1．吹き出しの外（アイドル画像の青など）は捨てる．
fn blue_text_mask(img: &RgbImage, blue_min: i64, margin: i64) -> Mask {
    let blue = Mask::from_fn(img, |p| p[2] as i64 >= blue_min && p[2] as i64 - p[0] as i64 >= margin);
    let both = Mask::from_fn(img, |p| p[0].min(p[1]).min(p[2]) >= 225 || (p[2] as i64 >= blue_min && p[2] as i64 - p[0] as i64 >= margin));
    // 文字の隙間を埋めて吹き出し全体を 1 つの塊にする
    let k = (blue.h / 3).max(3);
    let filled = morph_close(&both, k);
    let (_, comps) = connected_components(&filled);
    let mut best = None;
    let mut best_blue = 0;
    for c in comps {
        if c.w < c.h * 2 {
            continue; // 吹き出しは横長
        }
        let mut nb = 0;
        for y in c.y..c.y + c.h {
            for x in c.x..c.x + c.w {
                nb += blue.get(x, y) as usize;
            }
        }
        if nb > best_blue {
            best = Some(c);
            best_blue = nb;
        }
    }
    let mut out = Mask::zeros(blue.w, blue.h);
    if let Some(c) = best.filter(|_| best_blue >= 8) {
        for y in c.y..c.y + c.h {
            for x in c.x..c.x + c.w {
                out.d[y * out.w + x] = blue.d[y * blue.w + x];
            }
        }
    }
    out
}

// --------------------------------------------------------------------------
// 1 文字ずつの切り出しと正規化
// --------------------------------------------------------------------------
/// 数字 1 文字ずつの 2 値画像を左から順に返す（カンマ・"Pt"・"+" は除く）．
fn split_glyphs(mask: &Mask, kind: &str) -> Vec<Mask> {
    let (lab, comps) = connected_components(mask);
    if comps.is_empty() {
        return vec![];
    }
    let hmax = comps.iter().map(|c| c.h).max().unwrap_or(0) as f64;
    let ratio = if kind == "bonus" { 0.85 } else { 0.7 };
    let mut keep: Vec<usize> = (0..comps.len()).filter(|&i| comps[i].h as f64 >= ratio * hmax && comps[i].area >= 6).collect();
    keep.sort_by_key(|&i| comps[i].x);
    let mut out = Vec::new();
    for i in keep {
        let c = comps[i];
        let mut g = Mask::zeros(c.w, c.h);
        for y in 0..c.h {
            for x in 0..c.w {
                g.d[y * c.w + x] = (lab[(c.y + y) * mask.w + c.x + x] == i as u32 + 1) as u8;
            }
        }
        let exp = (0.72 * c.h as f64).max(1.0);
        let parts = py_round(c.w as f64 / exp);
        if c.w as f64 > 1.45 * exp && parts >= 2 {
            // 隣の数字とくっついている → インクの少ない列で分割
            let prof: Vec<usize> = (0..c.w).map(|x| (0..c.h).filter(|&y| g.get(x, y)).count()).collect();
            let w = c.w as i64;
            let mut cuts = vec![0i64];
            for j in 1..parts {
                let cc = py_round(w as f64 * j as f64 / parts as f64);
                let (lo, hi) = ((cc - 3).max(1), (w - 1).min(cc + 4));
                cuts.push(if hi > lo {
                    let seg = &prof[lo as usize..hi as usize];
                    let m = *seg.iter().min().unwrap();
                    lo + seg.iter().position(|&v| v == m).unwrap() as i64
                } else {
                    cc
                });
            }
            cuts.push(w);
            for ab in cuts.windows(2) {
                if ab[1] > ab[0] {
                    out.push(g.cols(ab[0] as usize, ab[1] as usize));
                }
            }
        } else {
            out.push(g);
        }
    }
    out
}

fn normalize_glyph(g: &Mask) -> Vec<f32> {
    let Some((x0, x1, y0, y1)) = g.bbox() else {
        return vec![0.0; FEAT];
    };
    let (w, h) = (x1 - x0 + 1, y1 - y0 + 1);
    let bw = w.max(py_round(h as f64 * 0.8) as usize);
    let off = (bw - w) / 2;
    let mut bx = vec![0f32; h * bw];
    for y in 0..h {
        for x in 0..w {
            bx[y * bw + off + x] = g.get(x0 + x, y0 + y) as u8 as f32;
        }
    }
    resize_area_f32(&bx, bw, h, GW, GH)
}

// --------------------------------------------------------------------------
// 数字の見本（npz 形式．Python 版と同じファイルを読み書きする）
// --------------------------------------------------------------------------
fn parse_npy(bytes: &[u8]) -> Result<(Vec<usize>, Vec<f64>), String> {
    if bytes.len() < 10 || &bytes[..6] != b"\x93NUMPY" {
        return Err("npy ではありません".into());
    }
    let (hlen, start) = if bytes[6] == 1 {
        (u16::from_le_bytes([bytes[8], bytes[9]]) as usize, 10)
    } else {
        (u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize, 12)
    };
    let header = std::str::from_utf8(&bytes[start..start + hlen]).map_err(|e| e.to_string())?;
    let descr = regex::Regex::new(r"'descr':\s*'([^']+)'").unwrap().captures(header).map(|c| c[1].to_string()).ok_or("descr なし")?;
    let shape: Vec<usize> = regex::Regex::new(r"'shape':\s*\(([^)]*)\)")
        .unwrap()
        .captures(header)
        .ok_or("shape なし")?[1]
        .split(',')
        .filter_map(|t| t.trim().parse().ok())
        .collect();
    let data = &bytes[start + hlen..];
    let vals: Vec<f64> = match descr.trim_start_matches(['<', '|', '=']) {
        "f4" => data.chunks_exact(4).map(|c| f32::from_le_bytes(c.try_into().unwrap()) as f64).collect(),
        "f8" => data.chunks_exact(8).map(|c| f64::from_le_bytes(c.try_into().unwrap())).collect(),
        "i1" => data.iter().map(|&b| b as i8 as f64).collect(),
        "u1" => data.iter().map(|&b| b as f64).collect(),
        "i2" => data.chunks_exact(2).map(|c| i16::from_le_bytes(c.try_into().unwrap()) as f64).collect(),
        "i4" => data.chunks_exact(4).map(|c| i32::from_le_bytes(c.try_into().unwrap()) as f64).collect(),
        "i8" => data.chunks_exact(8).map(|c| i64::from_le_bytes(c.try_into().unwrap()) as f64).collect(),
        d => return Err(format!("未対応の型 {d}")),
    };
    Ok((shape, vals))
}

fn npy_bytes(descr: &str, shape: &str, data: &[u8]) -> Vec<u8> {
    let mut header = format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': {shape}, }}");
    while (10 + header.len() + 1) % 64 != 0 {
        header.push(' ');
    }
    header.push('\n');
    let mut out = b"\x93NUMPY\x01\x00".to_vec();
    out.extend_from_slice(&(header.len() as u16).to_le_bytes());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(data);
    out
}

fn load_npz(bytes: &[u8]) -> Result<(Vec<Vec<f32>>, Vec<i8>), String> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| e.to_string())?;
    let mut read = |name: &str| -> Result<(Vec<usize>, Vec<f64>), String> {
        let mut buf = Vec::new();
        zip.by_name(name).map_err(|e| e.to_string())?.read_to_end(&mut buf).map_err(|e| e.to_string())?;
        parse_npy(&buf)
    };
    let (xs, xv) = read("X.npy")?;
    let (_, yv) = read("Y.npy")?;
    let cols = xs.get(1).copied().unwrap_or(FEAT);
    if cols != FEAT {
        return Err(format!("見本の大きさが違います（{cols}）"));
    }
    let x = xv.chunks_exact(FEAT).map(|c| c.iter().map(|&v| v as f32).collect()).collect();
    let y = yv.iter().map(|&v| v as i8).collect();
    Ok((x, y))
}

/// 数字の見本．内蔵の見本＋使っているうちに検算 OK だった数字を少しずつ追加（解像度の違いに追従）．
struct DigitTemplates {
    x: Vec<Vec<f32>>,
    y: Vec<i8>,
    user_x: Vec<Vec<f32>>,
    user_y: Vec<i8>,
    dirty: usize,
}

const PER_DIGIT_USER: usize = 40;

impl DigitTemplates {
    fn load() -> Self {
        let (mut x, mut y) = load_npz(BUILTIN_TEMPLATES).unwrap_or_else(|e| {
            eprintln!("[ocr] 内蔵の見本を読めません: {e}");
            (vec![], vec![])
        });
        let (mut user_x, mut user_y) = (vec![], vec![]);
        if let Ok(bytes) = fs::read(user_templates_path()) {
            match load_npz(&bytes) {
                Ok((ux, uy)) => {
                    x.extend(ux.iter().cloned());
                    y.extend(uy.iter().copied());
                    user_x = ux;
                    user_y = uy;
                }
                Err(e) => eprintln!("[ocr] 追加の見本を読めません: {e}"),
            }
        }
        DigitTemplates { x, y, user_x, user_y, dirty: 0 }
    }

    fn available(&self) -> bool {
        let mut seen = [false; 10];
        self.y.iter().filter(|&&d| (0..10).contains(&d)).for_each(|&d| seen[d as usize] = true);
        seen.iter().all(|&b| b)
    }

    /// 数字ごとの最小距離を近い順に返す．
    fn classify(&self, feat: &[f32]) -> Vec<(i64, f64)> {
        let mut best: Vec<(i64, f64)> = Vec::new();
        for (row, &d) in self.x.iter().zip(&self.y) {
            let dist: f64 = row.iter().zip(feat).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
            match best.iter_mut().find(|(k, _)| *k == d as i64) {
                Some(e) => {
                    if dist < e.1 {
                        e.1 = dist
                    }
                }
                None => best.push((d as i64, dist)),
            }
        }
        best.sort_by(|a, b| a.1.total_cmp(&b.1));
        best
    }

    fn learn(&mut self, feats: &[Vec<f32>], digits: &str) {
        for (f, ch) in feats.iter().zip(digits.chars()) {
            let d = ch.to_digit(10).unwrap_or(0) as i8;
            if self.user_y.iter().filter(|&&y| y == d).count() >= PER_DIGIT_USER {
                let i = self.user_y.iter().position(|&y| y == d).unwrap();
                self.user_x.remove(i);
                self.user_y.remove(i);
            }
            self.user_x.push(f.clone());
            self.user_y.push(d);
        }
        self.dirty += 1;
        if self.dirty >= 5 {
            self.flush();
        }
    }

    fn flush(&mut self) {
        if self.user_y.is_empty() || self.dirty == 0 {
            return;
        }
        let n = self.user_y.len();
        let xb: Vec<u8> = self.user_x.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
        let yb: Vec<u8> = self.user_y.iter().map(|&v| v as u8).collect();
        let write = || -> Result<(), String> {
            let path = user_templates_path();
            let _ = fs::create_dir_all(path.parent().unwrap());
            let mut buf = Cursor::new(Vec::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opt = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
                zw.start_file("X.npy", opt).map_err(|e| e.to_string())?;
                zw.write_all(&npy_bytes("<f4", &format!("({n}, {FEAT})"), &xb)).map_err(|e| e.to_string())?;
                zw.start_file("Y.npy", opt).map_err(|e| e.to_string())?;
                zw.write_all(&npy_bytes("|i1", &format!("({n},)"), &yb)).map_err(|e| e.to_string())?;
                zw.finish().map_err(|e| e.to_string())?;
            }
            fs::write(path, buf.into_inner()).map_err(|e| e.to_string())
        };
        match write() {
            Ok(()) => self.dirty = 0,
            Err(e) => eprintln!("[ocr] 見本の保存に失敗: {e}"),
        }
    }
}

impl Drop for DigitTemplates {
    fn drop(&mut self) {
        self.flush();
    }
}

// --------------------------------------------------------------------------
// エンジン（テンプレート照合が既定．見本が無い場合・指定された場合のみ Windows OCR）
// --------------------------------------------------------------------------
pub struct OcrEngine {
    pub name: &'static str,
    lang: String,
    templates: Mutex<DigitTemplates>,
}

impl OcrEngine {
    pub fn new(engine: &str, lang: &str) -> Result<Self, String> {
        let templates = DigitTemplates::load();
        let name = if matches!(engine, "auto" | "template") && templates.available() {
            "template"
        } else if matches!(engine, "auto" | "template" | "winocr") {
            winocr::available(lang)?;
            "winocr"
        } else {
            return Err(format!("読み取り方式「{engine}」には対応していません"));
        };
        Ok(OcrEngine { name, lang: lang.to_string(), templates: Mutex::new(templates) })
    }

    fn read_line(&self, img: &RgbImage) -> String {
        winocr::recognize(img, &self.lang)
            .map(|lines| lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>().join(" "))
            .unwrap_or_default()
    }
}

fn glyphs_to_image(glyphs: &[Mask], scale: u32) -> Option<RgbImage> {
    if glyphs.is_empty() {
        return None;
    }
    let h = glyphs.iter().map(|g| g.h).max().unwrap();
    let gap = (h / 4).max(2);
    let w = glyphs.iter().map(|g| g.w).sum::<usize>() + gap * (glyphs.len() - 1);
    let mut canvas = image::GrayImage::from_pixel(w as u32, h as u32, Luma([255]));
    let mut x = 0;
    for g in glyphs {
        for gy in 0..g.h {
            for gx in 0..g.w {
                if g.get(gx, gy) {
                    canvas.put_pixel((x + gx) as u32, (h - g.h + gy) as u32, Luma([0]));
                }
            }
        }
        x += g.w + gap;
    }
    let big = imageops::resize(&canvas, w as u32 * scale, h as u32 * scale, FilterType::Lanczos3);
    let mut out = RgbImage::from_pixel(big.width() + 48, big.height() + 48, image::Rgb([255, 255, 255]));
    for (px, py, p) in big.enumerate_pixels() {
        out.put_pixel(px + 24, py + 24, image::Rgb([p[0], p[0], p[0]]));
    }
    Some(out)
}

// --------------------------------------------------------------------------
// 結果画面の読み取り
// --------------------------------------------------------------------------
/// 1 つの数値欄の読み取り結果．候補 = [(値, コスト)]，コストが小さいほど確からしい．
#[derive(Default)]
struct FieldRead {
    value: Option<i64>,
    cands: Vec<(i64, f64)>,
    feats: Vec<Vec<f32>>,
    digits: String,
    xcenter: Option<f64>,
    empty: bool, // 空き枠（スコアの代わりに「ー」が表示されている）
}

fn digits_value(s: &str) -> i64 {
    // ノイズで桁が異常に多い時は範囲外の値にしておく
    if s.len() > 12 {
        10_i64.pow(13)
    } else {
        s.parse().unwrap_or(0)
    }
}

/// 空き枠の「ー」か．数字より明らかに背が低く横長の白い線だけがある状態．
fn is_dash(mask: &Mask) -> bool {
    if mask.count() < 4 {
        return false;
    }
    let (x0, x1, y0, y1) = mask.bbox().unwrap();
    let (w, h) = ((x1 - x0 + 1) as f64, (y1 - y0 + 1) as f64);
    h <= 0.35 * mask.h as f64 && w >= 2.5 * h
}

/// (値, コスト) の候補から範囲内のものを値ごとに最小コストでまとめ，コスト順に n 個．
fn top(c: &[(i64, f64)], lo: i64, hi: i64, n: usize) -> Vec<(i64, f64)> {
    let mut best: Vec<(i64, f64)> = Vec::new();
    for &(v, cost) in c {
        if !(lo..=hi).contains(&v) {
            continue;
        }
        match best.iter_mut().find(|(k, _)| *k == v) {
            Some(e) => {
                if cost < e.1 {
                    e.1 = cost
                }
            }
            None => best.push((v, cost)),
        }
    }
    best.sort_by(|a, b| a.1.total_cmp(&b.1));
    best.truncate(n);
    best
}

pub struct ResultReader {
    pub engine: OcrEngine,
    /// 検算 OK だった数字を見本に追加するか（比較テストでは止める）
    pub learn: bool,
}

impl ResultReader {
    pub fn new(engine: OcrEngine) -> Self {
        ResultReader { engine, learn: true }
    }

    // ---- 1 欄 ----
    fn read_field(&self, crop: &RgbImage, kind: &str, thr: i64, ocfg: &Value) -> FieldRead {
        let mask = if kind == "bonus" {
            blue_text_mask(crop, ocfg["bonus_blue_min"].as_i64().unwrap_or(190), ocfg["bonus_margin"].as_i64().unwrap_or(30))
        } else {
            white_text_mask(crop, thr)
        };
        if kind == "score" && is_dash(&mask) {
            return FieldRead { value: Some(0), cands: vec![(0, 0.0)], empty: true, ..Default::default() };
        }
        let glyphs = split_glyphs(&mask, kind);
        if glyphs.is_empty() {
            return FieldRead::default();
        }
        let xcenter = mask.bbox().map(|(x0, x1, _, _)| (x0 + x1) as f64 / 2.0 / mask.w as f64);
        if self.engine.name != "template" {
            let txt: String = glyphs_to_image(&glyphs, 3)
                .map(|img| self.engine.read_line(&img))
                .unwrap_or_default()
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect();
            let v = (!txt.is_empty()).then(|| digits_value(&txt));
            return FieldRead { value: v, cands: v.map(|v| vec![(v, 0.0)]).unwrap_or_default(), xcenter, ..Default::default() };
        }

        let feats: Vec<Vec<f32>> = glyphs.iter().map(normalize_glyph).collect();
        let ranked: Vec<Vec<(i64, f64)>> = {
            let t = self.engine.templates.lock().unwrap();
            feats.iter().map(|f| t.classify(f)).collect()
        };
        let digits: String = ranked.iter().map(|r| char::from_digit(r[0].0 as u32, 10).unwrap_or('0')).collect();
        let value = digits_value(&digits);
        let mut cands = vec![(value, 0.0)];
        let with = |i: usize, d: i64, s: &str| -> String {
            let mut ch: Vec<char> = s.chars().collect();
            ch[i] = char::from_digit(d as u32, 10).unwrap_or('0');
            ch.into_iter().collect()
        };
        // 1 文字だけ 2〜3 番目の候補に置き換えた値（コスト＝距離の増分）
        for (i, r) in ranked.iter().enumerate() {
            for &(d, dist) in r.iter().skip(1).take(2) {
                cands.push((digits_value(&with(i, d, &digits)), dist - r[0].1));
            }
        }
        // 2 文字置き換え（それぞれの 2 番目の候補）
        for i in 0..ranked.len() {
            for j in i + 1..ranked.len() {
                let (ri, rj) = (&ranked[i], &ranked[j]);
                if ri.len() > 1 && rj.len() > 1 {
                    let alt = with(j, rj[1].0, &with(i, ri[1].0, &digits));
                    cands.push((digits_value(&alt), (ri[1].1 - ri[0].1) + (rj[1].1 - rj[0].1)));
                }
            }
        }
        cands.sort_by(|a, b| a.1.total_cmp(&b.1));
        FieldRead { value: Some(value), cands, feats, digits, xcenter, empty: false }
    }

    fn score_crops(screen: &RgbImage, regions: &Value, stage: usize) -> Vec<RgbImage> {
        let [_, y, _, h] = region(&regions["score"][stage]);
        let cols: Vec<f64> = regions["score_columns"].as_array().map(|a| a.iter().map(f).collect()).unwrap_or_default();
        (0..3).map(|i| crop_rel(screen, [cols[i], y, cols[i + 1] - cols[i], h])).collect()
    }

    pub fn read(&self, screen: &RgbImage, cfg: &Value) -> Value {
        let (ocfg, regions) = (&cfg["ocr"], &cfg["regions"]);
        let stages: Vec<Value> = (0..3).map(|st| self.read_stage(screen, st, ocfg, regions)).collect();
        let statuses: Vec<&str> = stages.iter().map(|st| s(&st["status"])).collect();
        let overall = if statuses.contains(&"flagged") {
            "flagged"
        } else if statuses.contains(&"repaired") {
            "repaired"
        } else {
            "ok"
        };
        let flags = stages
            .iter()
            .enumerate()
            .filter(|(_, st)| !s(&st["flag"]).is_empty())
            .map(|(i, st)| format!("S{}:{}", i + 1, s(&st["flag"])))
            .collect::<Vec<_>>()
            .join(";");
        json!({"stages": stages, "status": overall, "flags": flags})
    }

    /// ゲームのルール
    ///     ボーナス = 最高スコア ÷ 5（切り捨て）
    ///     合計     = 3 人のスコア + ボーナス
    /// を使って検算・補正する．ボーナス欄の読み取りは補助（吹き出しの位置＝最高スコアの列も使う）．
    fn read_stage(&self, screen: &RgbImage, stage: usize, ocfg: &Value, regions: &Value) -> Value {
        let st = ocfg["score_threshold"].as_i64().unwrap_or(190);
        let tt = ocfg["total_threshold"].as_i64().unwrap_or(210);
        let score_crops = Self::score_crops(screen, regions, stage);
        let total_crop = crop_rel(screen, region(&regions["total"][stage]));
        let bregion = region(&regions["bonus"][stage]);
        let bonus_crop = crop_rel(screen, bregion);

        let sf: Vec<FieldRead> = score_crops.iter().map(|c| self.read_field(c, "score", st, ocfg)).collect();
        let tf = self.read_field(&total_crop, "total", tt, ocfg);
        let bf = self.read_field(&bonus_crop, "bonus", 0, ocfg);
        let primary = json!({"scores": sf.iter().map(|f| f.value).collect::<Vec<_>>(), "total": tf.value, "bonus": bf.value});

        // 吹き出しの位置から「1 位の列」を推定
        let cols: Vec<f64> = regions["score_columns"].as_array().map(|a| a.iter().map(f).collect()).unwrap_or_default();
        let mut top_col = None;
        if let Some(xc) = bf.xcenter {
            let x = bregion[0] + xc * bregion[2];
            for i in 0..3 {
                if cols[i] <= x && x < cols[i + 1] {
                    top_col = Some(i);
                }
            }
        }

        let empty: Vec<bool> = sf.iter().map(|f| f.empty).collect();
        let done = |sc: &[i64], status: &str, flag: &str| -> Value {
            let mx = *sc.iter().max().unwrap();
            let shown: Vec<Option<i64>> = sc.iter().enumerate().map(|(i, &v)| (!empty[i]).then_some(v)).collect(); // 空き枠は空欄で記録
            let mut out = json!({"scores": shown, "total": sc.iter().sum::<i64>() + mx.div_euclid(5), "bonus": mx.div_euclid(5),
                                 "status": status, "flag": flag});
            if status != "ok" {
                out["raw"] = primary.clone();
            }
            out
        };
        let consistent = |sc: &[i64], t: Option<i64>| -> bool {
            let ok = sc.iter().enumerate().all(|(i, &v)| (empty[i] && v == 0) || (SCORE_MIN..=SCORE_MAX).contains(&v));
            ok && t == Some(sc.iter().sum::<i64>() + sc.iter().max().unwrap().div_euclid(5))
        };

        if empty.iter().all(|&e| e) {
            return done(&[0, 0, 0], "ok", "empty_stage"); // 3 人とも空き（ステージに誰もいない）
        }
        if sf.iter().all(|f| f.value.is_some()) {
            let sc0: Vec<i64> = sf.iter().map(|f| f.value.unwrap()).collect();
            if consistent(&sc0, tf.value) {
                let mx = *sc0.iter().max().unwrap();
                let bonus_ok = bf.value.is_none() || bf.value == Some(mx.div_euclid(5));
                let col_ok = top_col.is_none_or(|c| sc0[c] == mx);
                if bonus_ok && col_ok {
                    let mut fields: Vec<&FieldRead> = sf.iter().collect();
                    fields.push(&tf);
                    if bf.value.is_some() {
                        fields.push(&bf);
                    }
                    self.learn(&fields);
                }
                return done(&sc0, "ok", "");
            }
        }

        // ---- 候補を集める（しきい値を変えた読み直しも候補に入れる）----
        let mut s_c: Vec<Vec<(i64, f64)>> = sf.iter().map(|f| f.cands.clone()).collect();
        let mut t_c = tf.cands.clone();
        for d in [-30, 30, -55] {
            for (i, c) in score_crops.iter().enumerate() {
                if let Some(v) = self.read_field(c, "score", st + d, ocfg).value {
                    s_c[i].push((v, 0.4));
                }
            }
            if let Some(v) = self.read_field(&total_crop, "total", tt + d, ocfg).value {
                t_c.push((v, 0.4));
            }
        }
        let s_c: Vec<Vec<(i64, f64)>> =
            s_c.iter().enumerate().map(|(i, c)| if empty[i] { vec![(0, 0.0)] } else { top(c, SCORE_MIN, SCORE_MAX, 10) }).collect();
        let t_c = top(&t_c, TOTAL_MIN, TOTAL_MAX, 12);
        let t_map: HashMap<i64, f64> = t_c.iter().copied().collect();
        let b_map: HashMap<i64, f64> = top(&bf.cands, 1, SCORE_MAX, 10).into_iter().collect();

        // 補正候補の追加チェック．ボーナス欄が読めていれば「最高スコア÷5」と一致すること，
        // 吹き出しの位置が分かれば その列が最高スコアであることを必須にする（None = 不採用）．
        let extra_cost = |sc: &[i64]| -> Option<f64> {
            let mx = *sc.iter().max().unwrap();
            if let Some(c) = top_col {
                if sc[c] != mx {
                    return None;
                }
            }
            if !b_map.is_empty() {
                return b_map.get(&mx.div_euclid(5)).copied();
            }
            Some(1.0) // ボーナス欄が読めない場合は少し減点
        };

        let mut found: Vec<(f64, Vec<i64>, String)> = Vec::new();

        // A) 3 人とも候補あり：合計の候補と一致する組み合わせ
        if s_c.iter().all(|c| !c.is_empty()) {
            for a in &s_c[0] {
                for b in &s_c[1] {
                    for c in &s_c[2] {
                        let sc = [a.0, b.0, c.0];
                        let t = sc.iter().sum::<i64>() + sc.iter().max().unwrap().div_euclid(5);
                        if let (Some(tc), Some(ec)) = (t_map.get(&t), extra_cost(&sc)) {
                            found.push((a.1 + b.1 + c.1 + tc + ec, sc.to_vec(), "reread".into()));
                        }
                    }
                }
            }
        }

        // B) 1 人だけ読めない／候補が全部外れ：合計から逆算
        for miss in 0..3 {
            if empty[miss] {
                continue;
            }
            let others: Vec<usize> = (0..3).filter(|&j| j != miss).collect();
            if others.iter().any(|&j| s_c[j].is_empty()) {
                continue;
            }
            for o0 in &s_c[others[0]] {
                for o1 in &s_c[others[1]] {
                    let ov = [o0.0, o1.0];
                    let omax = ov[0].max(ov[1]);
                    let ocost = o0.1 + o1.1;
                    for &(t, tcost) in &t_c {
                        let rest = t - ov[0] - ov[1];
                        let mut sols = std::collections::BTreeSet::new();
                        let m = rest - omax.div_euclid(5); // 読めない人が 1 位ではない場合
                        if (SCORE_MIN..=omax).contains(&m) && omax > 0 {
                            sols.insert(m);
                        }
                        let c = (rest * 5).div_euclid(6);
                        for m in c - 2..c + 3 {
                            // 1 位の場合 m + m//5 = rest
                            if m >= omax && m + m.div_euclid(5) == rest {
                                sols.insert(m);
                            }
                        }
                        for m in sols {
                            let mut sc = ov.to_vec();
                            sc.insert(miss, m);
                            // 逆算は式 1 本で必ず解けてしまうので，ボーナス欄との一致を必須にする
                            if let Some(ec) = extra_cost(&sc) {
                                if !b_map.is_empty() {
                                    found.push((ocost + tcost + 2.5 + ec, sc, format!("derived_C{}", miss + 1)));
                                }
                            }
                        }
                    }
                }
            }
        }

        // C) 合計が読めない：3 人のスコアから計算（ボーナス欄が一致する場合のみ）
        if t_c.is_empty() && s_c.iter().all(|c| !c.is_empty()) && !b_map.is_empty() {
            for a in &s_c[0] {
                for b in &s_c[1] {
                    for c in &s_c[2] {
                        let sc = [a.0, b.0, c.0];
                        if let Some(ec) = extra_cost(&sc) {
                            found.push((a.1 + b.1 + c.1 + 2.5 + ec, sc.to_vec(), "total_derived".into()));
                        }
                    }
                }
            }
        }

        found.retain(|f| f.0 <= 8.0);
        if !found.is_empty() {
            found.sort_by(|a, b| a.0.total_cmp(&b.0));
            let (cost, sc, flag) = &found[0];
            // 同じくらい確からしい別解があれば念のため要確認に回す
            let rivals = found[1..].iter().any(|f| &f.1 != sc && f.0 - cost < 0.15);
            if rivals && *cost > 0.5 {
                return done(sc, "flagged", &format!("{flag}_ambiguous"));
            }
            return done(sc, "repaired", flag);
        }

        let sc: Vec<Option<i64>> = s_c.iter().enumerate().map(|(i, c)| if empty[i] { None } else { c.first().map(|v| v.0) }).collect();
        json!({"scores": sc, "total": t_c.first().map(|v| v.0), "bonus": bf.value,
               "status": "flagged", "flag": "mismatch", "raw": primary})
    }

    fn learn(&self, fields: &[&FieldRead]) {
        if self.engine.name != "template" || !self.learn {
            return;
        }
        let mut t = self.engine.templates.lock().unwrap();
        for f in fields {
            if !f.feats.is_empty() {
                t.learn(&f.feats, &f.digits);
            }
        }
    }

    /// レビュー画面用：そのステージの合計〜スコア行〜ボーナス行までの切り抜き．
    pub fn stage_crop(screen: &RgbImage, cfg: &Value, stage: usize) -> RgbImage {
        let r = &cfg["regions"];
        let (t, sc, b) = (region(&r["total"][stage]), region(&r["score"][stage]), region(&r["bonus"][stage]));
        let y0 = t[1] - 0.005;
        let y1 = (sc[1] + sc[3]).max(b[1] + b[3]) + 0.005;
        crop_rel(screen, [0.22, y0, 0.56, y1 - y0])
    }
}
